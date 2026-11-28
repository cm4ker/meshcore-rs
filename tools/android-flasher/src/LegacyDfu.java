package ru.ommesh.otg;

import java.io.*;
import java.util.Arrays;

final class LegacyDfu {
    interface Link {
        void control(byte... value)throws IOException;
        void packet(byte[] value)throws IOException;
        byte[] notice()throws IOException;
    }
    static void response(Link link,int op)throws IOException{
        byte[] r=link.notice();
        checkResponse(link,r,op);
    }
    static String hex(byte[] bytes){
        StringBuilder text=new StringBuilder();
        for(byte b:bytes)text.append(String.format(java.util.Locale.ROOT,"%02X ",b&255));
        return text.toString().trim();
    }
    static void checkResponse(Link link,byte[] r,int op)throws IOException{
        Ota.need(r.length==3 && r[0]==16 && r[1]==op,"Неожиданный ответ DFU на команду "+op+": "+hex(r));
        if(op==1 && r[2]==2){
            try{
                link.control((byte)6);
            }catch(IOException e){
                throw new IOException("Загрузчик занят (статус 2). Перезапустите плату, откройте BLE OTA и повторите поиск. "+e.getMessage(),e);
            }
            throw new IOException("Загрузчик занят (статус 2). Отправлена команда перезапуска. Снова откройте BLE OTA на плате, нажмите «Найти загрузчик», затем «Начать».");
        }
        Ota.need(r[2]==1,"Загрузчик отклонил команду "+op+" (статус "+(r[2]&255)+").");
    }
    static void receipt(Link link,byte[] r,int sent)throws IOException{
        if(r.length==3 && r[0]==16){
            checkResponse(link,r,3);
            throw new IOException("Загрузчик преждевременно завершил приём после "+sent+" байт.");
        }
        Ota.need(r.length>=5 && r[0]==17,"Ожидалось подтверждение "+sent+" байт. Ответ DFU: "+hex(r));
        long received=Ota.i32(r,1)&0xffffffffL;
        Ota.need(received==sent,"Передано "+sent+" байт, загрузчик подтвердил "+received+". Ответ DFU: "+hex(r));
    }
    static void send(Link link,LegacyPackage pkg,Ota.Log log)throws IOException{
        send(link,pkg,log,Ota.Progress.NONE);
    }
    static void send(Link link,LegacyPackage pkg,Ota.Log log,Ota.Progress progress)throws IOException{
        log.show("Согласование начала DFU…");
        progress.update("Подключение к загрузчику",0,pkg.image.length);
        link.control((byte)1,(byte)4);
        link.packet(Ota.le(12).putInt(0).putInt(0).putInt(pkg.image.length).array());
        response(link,1);
        link.control((byte)2,(byte)0);link.packet(pkg.init);link.control((byte)2,(byte)1);
        response(link,2);
        log.show("Загрузчик принял параметры образа. Передача с подтверждением каждого пакета…");
        // Acknowledge every packet to limit pressure on bootloaders with small flash queues.
        final int interval=1;
        link.control((byte)8,(byte)interval,(byte)0);
        link.control((byte)3);
        progress.update("Передача прошивки",0,pkg.image.length);
        int packets=0,last=0;
        for(int pos=0;pos<pkg.image.length;){
            int next=Math.min(pos+20,pkg.image.length);
            link.packet(Arrays.copyOfRange(pkg.image,pos,next));pos=next;
            if(++packets%interval==0 && pos<pkg.image.length){
                byte[] r=link.notice();
                receipt(link,r,pos);
                progress.update("Передача прошивки",pos,pkg.image.length);
            }
            int percent=pos*100/pkg.image.length;
            if(percent/10>last){last=percent/10;log.show("Передано "+percent+" %");}
        }
        byte[] completed=link.notice();
        if(completed.length>0 && completed[0]==17){
            receipt(link,completed,pkg.image.length);
            completed=link.notice();
        }
        checkResponse(link,completed,3);
        progress.update("Проверка образа",pkg.image.length,pkg.image.length);
        link.control((byte)4);response(link,4);
        log.show("Загрузчик подтвердил образ.");
        link.control((byte)5);
        log.show("Отправлена команда перезагрузки. Проверьте версию после запуска платы.");
    }
}
