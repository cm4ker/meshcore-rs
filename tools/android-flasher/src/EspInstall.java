package ru.ommesh.otg;
import java.io.*;
import java.nio.*;
import java.security.*;
import java.util.*;
import java.util.zip.*;
import static ru.ommesh.otg.Ota.*;

class EspInstall {
    interface Wire extends Port {void dtr(boolean on)throws IOException;void rts(boolean on)throws IOException;}
    interface Assets {InputStream open(String name)throws IOException;}
    final Wire wire;final Assets assets;final Log log;final boolean jtag;
    final byte[] buf=new byte[8192];int at,end;boolean stub;
    EspInstall(Wire w,Assets a,Log l,boolean j){wire=w;assets=a;log=l;jtag=j;}
    static byte[] ints(int... values){ByteBuffer b=le(values.length*4);for(int v:values)b.putInt(v);return b.array();}
    static byte[] join(byte[] a,byte[] b){byte[] x=Arrays.copyOf(a,a.length+b.length);System.arraycopy(b,0,x,a.length,b.length);return x;}
    static byte[] load(InputStream raw)throws IOException{try(InputStream in=raw;ByteArrayOutputStream out=new ByteArrayOutputStream()){byte[] b=new byte[8192];int n;while((n=in.read(b))!=-1)out.write(b,0,n);return out.toByteArray();}}
    static byte[] md5(byte[] a)throws Exception{return MessageDigest.getInstance("MD5").digest(a);}
    void slip(byte[] data)throws IOException{
        ByteArrayOutputStream o=new ByteArrayOutputStream();o.write(0xc0);
        for(byte v:data){if(v==(byte)0xc0){o.write(0xdb);o.write(0xdc);}else if(v==(byte)0xdb){o.write(0xdb);o.write(0xdd);}else o.write(v);}
        o.write(0xc0);wire.write(o.toByteArray());
    }
    byte[] packet(long until)throws IOException{
        ByteArrayOutputStream o=new ByteArrayOutputStream();boolean started=false,escape=false;
        while(System.nanoTime()<until){
            if(at>=end){end=wire.read(buf,300);at=0;if(end<=0)continue;}
            int v=buf[at++]&255;
            if(v==0xc0){if(started&&o.size()>0)return o.toByteArray();started=true;escape=false;continue;}
            if(!started)continue;
            if(escape){need(v==0xdc||v==0xdd,"Повреждён пакет загрузчика.");v=v==0xdc?0xc0:0xdb;escape=false;}
            else if(v==0xdb){escape=true;continue;}
            need(o.size()<32768,"Слишком длинный пакет загрузчика.");o.write(v);
        }
        throw new java.net.SocketTimeoutException("Нет ответа загрузчика ESP.");
    }
    byte[] command(int op,byte[] body,int xor,int ms)throws IOException {
        slip(join(le(8).put((byte)0).put((byte)op).putShort((short)body.length).putInt(xor).array(),body));
        long deadline=System.nanoTime()+ms*1000000L;int tail=stub?2:4;
        for(;;){byte[] b=packet(deadline);
            if(b.length<8+tail||b[0]!=1||(b[1]&255)!=op)continue;
            need(u16(b,2)==b.length-8,"Длина пакета ESP не совпала.");
            need(b[b.length-tail]==0,"Загрузчик отклонил команду "+Integer.toHexString(op)+".");
            return Arrays.copyOfRange(b,8,b.length-tail);
        }
    }
    static int xor(byte[] data){int n=0xef;for(byte x:data)n^=x&255;return n;}
    void startBoot(int chip,String chipName)throws Exception{
        if(jtag){wire.rts(false);wire.dtr(false);Thread.sleep(100);wire.dtr(true);Thread.sleep(100);wire.rts(true);wire.dtr(false);Thread.sleep(100);wire.rts(false);}
        else{wire.dtr(false);wire.rts(true);Thread.sleep(100);wire.dtr(true);wire.rts(false);Thread.sleep(100);wire.dtr(false);}
        Thread.sleep(80);
        byte[] sync=new byte[36];Arrays.fill(sync,(byte)0x55);sync[0]=7;sync[1]=7;sync[2]=18;sync[3]=32;
        for(int attempt=0;;attempt++){try{command(8,sync,0,900);break;}catch(java.net.SocketTimeoutException ex){if(attempt>=4)throw ex;}}
        byte[] sec=command(0x14,new byte[0],0,3000);
        need(sec.length>=16&&i32(sec,12)==chip,"Образ не соответствует чипу.");
        need((i32(sec,0)&5)==0&&(Integer.bitCount(sec[4]&255)%2)==0,"Включена аппаратная защита ESP.");
        ByteBuffer blob=ByteBuffer.wrap(load(assets.open(chipName+".stub"))).order(ByteOrder.LITTLE_ENDIAN);
        int entry=blob.getInt();
        for(int seg=0;seg<2;seg++){int addr=blob.getInt(),length=blob.getInt();command(5,ints(length,(length+1023)/1024,1024,addr),0,4000);
            for(int k=0;k<length;k+=1024){byte[] block=new byte[Math.min(1024,length-k)];blob.get(block);
                command(7,join(ints(block.length,k/1024,0,0),block),xor(block),4000);}}
        // Some ROMs emit the stub greeting before the MEM_END response is complete.
        slip(join(le(8).put((byte)0).put((byte)6).putShort((short)8).putInt(0).array(),ints(0,entry)));
        long deadline=System.nanoTime()+6000000000L;
        while(!Arrays.equals(packet(deadline),new byte[]{79,72,65,73})){}
        stub=true;
    }
    byte[] read(int offset,int size)throws Exception{
        command(0xd2,ints(offset,size,1024,1),0,4000);
        byte[] out=new byte[size];int used=0;
        while(used<size){byte[] b=packet(System.nanoTime()+6000000000L);need(b.length==Math.min(1024,size-used),"Неполное чтение флеш-памяти.");
            System.arraycopy(b,0,out,used,b.length);used+=b.length;slip(ints(used));}
        need(Arrays.equals(packet(System.nanoTime()+6000000000L),md5(out)),"Ошибка проверки считанной памяти.");return out;
    }
    void verify(int addr,byte[] bytes)throws Exception{
        need(Arrays.equals(md5(bytes),command(0x13,ints(addr,bytes.length,0,0),0,30000)),"Содержимое флеш-памяти не прошло проверку.");
    }
    void write(int addr,byte[] bytes)throws Exception{
        write(addr,bytes,Progress.NONE);
    }
    void write(int addr,byte[] bytes,Progress progress)throws Exception{
        final int block=4096,n=(bytes.length+block-1)/block;
        command(2,ints(bytes.length,n,block,addr),0,30000);
        progress.update("Запись образа ESP",0,n);
        for(int i=0;i<n;i++){
            byte[] b=new byte[block];Arrays.fill(b,(byte)0xff);System.arraycopy(bytes,i*block,b,0,Math.min(block,bytes.length-i*block));
            command(3,join(ints(block,i,0,0),b),xor(b),15000);
            progress.update("Запись образа ESP",i+1,n);
            if(n>1&&(i+1)%32==0)log.show("Запись: "+(i+1)*100/n+" %");
        }
        progress.update("Проверка образа ESP",n,n);
        verify(addr,bytes);
    }
    static byte[] validate(byte[] file,int board,int chip)throws Exception{
        byte[] app=file;
        if(file.length>=4&&file[0]=='M'&&file[1]=='C'&&file[2]=='F'&&file[3]=='W'){
            need(file.length>=54&&file[4]==1&&(file[5]&255)==board,"Файл MCFW другой модели или повреждён.");
            int at=53+(file[52]&255);need(at<file.length,"Неполный файл.");
            at+=1+96*(file[at]&255);need(at<file.length&&file.length-at==i32(file,12),"Неверный размер образа.");
            app=Arrays.copyOfRange(file,at,file.length);need(Arrays.equals(digest(app),Arrays.copyOfRange(file,16,48)),"Ошибка SHA-256 выпуска.");
        }
        need(app.length>64&&(app[0]&255)==0xe9&&u16(app,12)==chip&&i32(app,32)==0xabcd5432&&app[23]==1,"Нужен образ приложения для этой платы.");
        int p=24,sum=0xef,segments=app[1]&255;need(segments>0&&segments<=16,"Неверное число сегментов ESP.");
        for(int i=0;i<segments;i++){need(p+8<=app.length,"Оборван сегмент.");int n=i32(app,p+4);p+=8;need(n>=0&&n<=app.length-p,"Неверная длина сегмента.");for(int j=0;j<n;j++)sum^=app[p++]&255;}
        p|=15;need(p+33==app.length&&(app[p]&255)==sum,"Неверная контрольная сумма приложения.");
        need(Arrays.equals(digest(Arrays.copyOf(app,p+1)),Arrays.copyOfRange(app,p+1,app.length)),"Неверная SHA-256 приложения.");return app;
    }
    Map<String,int[]> layout(byte[] raw,String name)throws Exception{
        Map<String,int[]> parts=new HashMap<>();
        try(BufferedReader r=new BufferedReader(new InputStreamReader(assets.open(name+".csv"),"UTF-8"))){
            String row;while((row=r.readLine())!=null){row=row.trim();if(row.isEmpty()||row.startsWith("#"))continue;
                String[] c=row.split(",");String label=c[0].trim();int type=c[1].trim().equals("app")?0:1;
                String sub=c[2].trim();int subtype=sub.equals("ota_0")?16:sub.equals("ota_1")?17:sub.equals("nvs")?2:sub.equals("ota")?0:6;
                parts.put(label,new int[]{Integer.decode(c[3].trim()),Integer.decode(c[4].trim()),type,subtype});}}
        Set<String> seen=new HashSet<>();
        for(int p=0;p+32<=raw.length;p+=32){int magic=u16(raw,p);if(magic==0xffff)break;
            if(magic==0xebeb){need(Arrays.equals(md5(Arrays.copyOf(raw,p)),Arrays.copyOfRange(raw,p+16,p+32)),"Повреждена таблица разделов.");break;}
            String label=field(raw,p+12,p+28);int[] x=parts.get(label);
            need(magic==0x50aa&&x!=null&&seen.add(label)&&x[0]==i32(raw,p+4)&&x[1]==i32(raw,p+8)&&x[2]==(raw[p+2]&255)&&x[3]==(raw[p+3]&255)&&i32(raw,p+28)==0,"Неподдерживаемая разметка платы.");}
        need(seen.size()==parts.size(),"Неполная таблица разделов.");return parts;
    }
    static int seqCrc(int x){int crc=0;for(int i=0;i<4;i++){crc^=(x>>>(8*i))&255;for(int j=0;j<8;j++)crc=(crc>>>1)^((crc&1)!=0?0xedb88320:0);}return ~crc;}
    void install(byte[] file,Ota.Device d,boolean clean)throws Exception{
        install(file,d,clean,Progress.NONE);
    }
    void install(byte[] file,Ota.Device d,boolean clean,Progress progress)throws Exception{
        String board=d.board==8?"t3s3":d.board==1?"heltec-v4":d.board==2?"heltec-v3":d.board==3?"xiao-c6":d.board==6?"heltec-paper":null;
        need(board!=null,"Эта ESP-плата не поддерживается загрузчиком.");
        int chip=d.board==3?13:9;
        byte[] app=validate(file,d.board,chip);
        try{
            progress.update("Вход в загрузчик ESP",0,app.length);
            log.show("Вход в загрузчик "+board+"…");startBoot(chip,chip==9?"esp32s3":"esp32c6");
            progress.update("Проверка разделов ESP",0,app.length);
            Map<String,int[]> pt=layout(read(0x8000,4096),board);int meta=pt.get("otadata")[0];
            byte[] records=read(meta,8192);long best=0;int sector=1;
            for(int s=0;s<2;s++){int p=s*4096,q=i32(records,p),state=i32(records,p+24);
                if(q==-1)continue;long seq=q&0xffffffffL;need(seq>0&&seq<0xfffffffdL&&i32(records,p+28)==seqCrc(q),"Повреждены данные выбора слота.");
                if((state==-1||(state>=0&&state<=2))&&seq>=best){best=seq;sector=s;}}
            if(best==0)best=1;int active=(int)((best-1)%2);
            int[] old=pt.get("ota_"+active),next=pt.get("ota_"+(1-active));
            need(d.runningSize>0&&d.runningSize<=old[1]&&app.length<=next[1],"Неверный размер раздела или прошивки.");
            need(Arrays.equals(Arrays.copyOf(digest(read(old[0],d.runningSize)),8),d.hash),"Активный слот не подтверждён.");
            write(next[0],app,progress);
            if(clean)progress.update("Очистка данных ноды",app.length,app.length);
            if(clean)for(String key:new String[]{"nvs","meshcore","companion"}){int[] x=pt.get(key);log.show("Очистка "+key+"…");command(0xd1,ints(x[0],x[1]),0,60000);byte[] blank=new byte[x[1]];Arrays.fill(blank,(byte)0xff);verify(x[0],blank);}
            progress.update("Выбор нового слота ESP",app.length,app.length);
            int seq=(int)(best+1);byte[] flag=new byte[32];Arrays.fill(flag,(byte)0xff);ByteBuffer b=ByteBuffer.wrap(flag).order(ByteOrder.LITTLE_ENDIAN);
            b.putInt(0,seq).putInt(24,2).putInt(28,seqCrc(seq));write(meta+(1-sector)*4096,flag);
        }finally{wire.dtr(false);wire.rts(true);Thread.sleep(200);wire.rts(false);}
        log.show("Запись проверена. После перезагрузки нажмите «Проверить устройство».");
    }
}

