package ru.ommesh.otg;

import java.io.*;
import java.nio.*;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.*;
import java.util.zip.*;

final class Ota {
    interface Port { void write(byte[] frame) throws IOException; int read(byte[] buffer,int ms) throws IOException; }
    interface Log { void show(String message); }
    final Port port;
    final Log log;
    final byte[] input=new byte[1024];
    int pos, end;
    static final int CHUNK=168;
    Ota(Port port, Log log){this.port=port;this.log=log;}
    static ByteBuffer le(int n){return ByteBuffer.allocate(n).order(ByteOrder.LITTLE_ENDIAN);}
    static int u16(byte[] a,int i){return (a[i]&255)|((a[i+1]&255)<<8);}
    static int i32(byte[] a,int i){return ByteBuffer.wrap(a,i,4).order(ByteOrder.LITTLE_ENDIAN).getInt();}
    static void need(boolean ok,String m) throws IOException {if(!ok)throw new IOException(m);}
    static byte[] digest(byte[] a) throws Exception {return MessageDigest.getInstance("SHA-256").digest(a);}
    static String field(byte[] a,int start,int limit){int i=start;while(i<a.length && i<limit && a[i]!=0)i++;return i==start?"":new String(a,start,i-start,StandardCharsets.UTF_8);}
    void send(int op,byte[] data)throws IOException {
        port.write(le(4+data.length).put((byte)'<').putShort((short)(1+data.length)).put((byte)op).put(data).array());
    }
    int next(long until)throws IOException {
        while(pos>=end){
            long ms=(until-System.nanoTime())/1000000;
            if(ms<=0)throw new java.net.SocketTimeoutException("Плата не ответила вовремя.");
            end=port.read(input,(int)Math.min(ms,500));pos=0;
        }
        return input[pos++]&255;
    }
    byte[] request(int op,byte[] data,int timeout)throws IOException {
        send(op,data);
        long until=System.nanoTime()+timeout*1000000L;
        for(;;){
            if(next(until)!='>')continue;
            int n=next(until)|(next(until)<<8);
            if(n<1||n>300)continue;
            int code=next(until);
            byte[] reply=new byte[n-1];
            for(int i=0;i<reply.length;i++)reply[i]=(byte)next(until);
            if(code==(op==22?13:op))return reply;
        }
    }
    static final class Device {
        int runningSize, capacity, board=-1,role=-1,build,roots; byte[] hash;
        String model="",version="";

        String summary() {
            String identity=model.isEmpty()?"Устройство MeshCore":model;
            if(!version.isEmpty())identity+=" · "+version;
            String details=board>=0?"Сборка №"+(build&0xffffffffL)+" · ":"";
            if(role==1)details+="Репитер · ";
            if(role==2)details+="Компаньон · ";
            return identity+"\n"+details+"слот "+capacity/1024+" КиБ";
        }
    }
    Device info() throws IOException {
        byte[] dev=request(22,new byte[]{3},10000);
        need(dev.length>=59,"Неполный ответ с моделью устройства.");
        Device d=new Device();
        d.model=field(dev,19,59);
        d.version=field(dev,59,79);
        log.show(d.model+" · "+d.version);
        byte[] a=request(0xf5,new byte[0],15000);
        need(a.length>=5,"Плата не вернула параметры OTA.");
        need(a[0]!=0,"У устройства нет слота обновления.");
        d.capacity=i32(a,1);
        int k=5;while(k<a.length&&a[k]!=0)k++;k++;
        if(a.length-k>=12){d.runningSize=i32(a,k);d.hash=Arrays.copyOfRange(a,k+4,k+12);}
        if(a.length-k>=19){d.board=a[k+12]&255;d.role=a[k+13]&255;d.build=i32(a,k+14);d.roots=a[k+18]&255;log.show("Сборка №"+d.build+" · слот "+d.capacity/1024+" КиБ");}
        return d;
    }
    static void accepted(byte[] reply,String stage)throws IOException {
        need(reply.length>=1,"Пустой ответ платы.");
        int code=reply[0]&255;
        if(code!=0){
            String why=code==9?"исходный образ на плате не подходит для этой дельты":code==13?"выбранная сборка старее установленной":code==12?"другая плата или роль":code==11?"неподходящая подпись":code==10?"нужен подписанный выпуск":"код "+code;
            throw new IOException(stage+" — "+why+".");
        }
    }
    byte[] status() throws IOException {
        for(int i=0;;i++){
            try{byte[] r=request(0xf2,new byte[]{0,0},5000);accepted(r,"Статус OTA");need(r.length>=6,"Неполный статус OTA.");return r;}
            catch(java.net.SocketTimeoutException e){if(i==3)throw e;}
        }
    }
    static int aligned(int n){return (n+4095)&~4095;}
    void update(byte[] file) throws Exception {
        update(file,false);
    }
    void update(byte[] file,boolean delta) throws Exception {
        Device device=info();
        need(file.length>0,"Пустой файл.");
        boolean signed=file.length>=4&&file[0]=='M'&&file[1]=='C'&&file[2]=='F'&&file[3]=='W';
        need(!delta||signed,"Повреждён заголовок MCFD.");
        int env=0, imageSize=file.length;
        byte[] image=file, hash;
        if(signed){
            need(file.length>=54&&file[4]==1&&(file[52]&255)<=31,"Повреждён заголовок выпуска.");
            int n=53+(file[52]&255);
            need(n<file.length&&(file[n]&255)<=8,"Повреждён конверт выпуска.");
            env=n+1+96*(file[n]&255);
            imageSize=i32(file,12);
            need(env<file.length&&imageSize>0&&(delta||imageSize<=device.capacity),"Размер выпуска не подходит для слота.");
            hash=Arrays.copyOfRange(file,16,48);
            if(device.board>=0)need((file[5]&255)==device.board&&(file[6]&255)==device.role,"Выпуск предназначен для другой платы или роли.");
            if(!delta){
                need(file.length-env==imageSize,"Неполный файл MCFW.");
                image=Arrays.copyOfRange(file,env,file.length);
                need(Arrays.equals(hash,digest(image)),"Неверная SHA-256 образа.");
            }
            log.show("Выбран выпуск №"+i32(file,8));
        }else{
            need(device.roots==0,"Плата принимает только подписанные выпуски.");
            hash=digest(image);
        }
        need(delta||imageSize<=device.capacity,"Образ больше слота обновления.");
        need(!Arrays.equals(Arrays.copyOf(hash,8),device.hash),"Этот образ уже установлен.");
        byte[] payload, begin;
        if(delta){
            need(device.board>=0&&file.length>=env+38,"Неполный MCFD или плата не поддерживает этот формат.");
            int baseSize=i32(file,env);
            byte[] baseHash=Arrays.copyOfRange(file,env+4,env+36);
            need(baseSize>0&&baseSize==device.runningSize&&Arrays.equals(Arrays.copyOf(baseHash,8),device.hash),
                 "MCFD рассчитан на другую исходную прошивку. Выберите подходящую дельту или полный .mcfw.");
            payload=Arrays.copyOfRange(file,env+36,file.length);

            begin=le(79).putInt(payload.length).put(hash).putShort((short)CHUNK).put((byte)7)
                .putInt(imageSize).putInt(baseSize).put(baseHash).array();
        }else{
            ByteArrayOutputStream compressed=new ByteArrayOutputStream();
            Deflater z=new Deflater(9);
            try(DeflaterOutputStream out=new DeflaterOutputStream(compressed,z)){out.write(image);}finally{z.end();}
            boolean useZip=aligned(compressed.size())+aligned(image.length)<=device.capacity;
            payload=useZip?compressed.toByteArray():image;
            begin=le(43).putInt(payload.length).put(hash).putShort((short)CHUNK).put((byte)(useZip?5:4)).putInt(useZip?image.length:0).array();
        }
        if(signed&&device.board>=0){
            for(int off=0;off<env;off+=160){
                int len=Math.min(160,env-off);
                byte[] b=le(5+len).putShort((short)off).putShort((short)env).put((byte)len).put(file,off,len).array();
                accepted(request(0xf6,b,15000),"Плата отклонила выпуск");
            }
        }
        int total=(payload.length+CHUNK-1)/CHUNK;
        need(total>0&&total<=65535,"Слишком большой образ.");
        accepted(request(0xf0,begin,30000),"Начало OTA");
        log.show("Передача прошивки…");
        int[] missing=new int[total];for(int i=0;i<total;i++)missing[i]=i;
        boolean complete=false;
        for(int round=0;round<50&&!complete;round++){
            for(int k=0;k<missing.length;k++){
                int part=missing[k],off=part*CHUNK,len=Math.min(CHUNK,payload.length-off);
                byte[] body=le(len+6).putShort((short)part).put(payload,off,len).array();
                CRC32 crc=new CRC32();crc.update(body,0,len+2);
                ByteBuffer.wrap(body).order(ByteOrder.LITTLE_ENDIAN).putInt(len+2,(int)crc.getValue());
                send(0xf1,body);
                if(round==0&&(k+1)%500==0){byte[] r=status();log.show("Принято "+u16(r,1)+" из "+total+" блоков");}
            }
            byte[] r=status();int have=u16(r,1),count=r[5]&255;
            need(u16(r,3)==total&&have<=total,"Некорректное число блоков в ответе.");
            complete=have==total;
            if(!complete){
                need(count>0&&r.length>=6+2*count,"Плата не вернула пропущенные блоки.");
                missing=new int[count];for(int i=0;i<count;i++){missing[i]=u16(r,6+2*i);need(missing[i]<total,"Некорректный номер блока.");}
            }
        }
        need(complete,"Плата не получила все блоки.");
        log.show("Плата проверяет полученный образ…");
        byte[] commit;
        try {commit=request(0xf3,new byte[0],120000);}
        catch(IOException lost){throw new IOException("Ответ после записи не получен. После перезагрузки нажмите «Проверить устройство» и сверьте сборку.",lost);}
        accepted(commit,"Завершение OTA");
        log.show("Плата подтвердила запись. Дождитесь перезагрузки.");
    }
}

