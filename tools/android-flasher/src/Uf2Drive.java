package ru.ommesh.otg;

import android.content.ContentResolver;
import android.database.Cursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.DocumentsContract;
import android.provider.DocumentsContract.Document;
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.Locale;

/** Android mounts UF2 bootloaders as removable volumes and writes the original UF2 bytes. */
final class Uf2Drive {
    private static final String EXTERNAL_STORAGE="com.android.externalstorage.documents";
    private static final String INFO_FILE="INFO_UF2.TXT";
    final Uri tree;
    final Info info;

    static final class Info {
        final String model, boardId, version, softDevice;
        final DfuBoard board;

        Info(String model,String boardId,String version,String softDevice) {
            this.model=model;
            this.boardId=boardId;
            this.version=version;
            this.softDevice=softDevice;
            board=DfuBoard.fromUsbIdentity(model,boardId);
        }

        String summary() {
            String title=board!=null?board.title:(model.isEmpty()?boardId:model);
            return title+"\nUF2-загрузчик "+version+(softDevice.isEmpty()?"":" · "+softDevice);
        }

        boolean sameAs(Info other) {
            return model.equals(other.model)&&boardId.equals(other.boardId)&&
                version.equals(other.version)&&softDevice.equals(other.softDevice);
        }

        void checkFirmwareName(String filename)throws IOException {
            DfuBoard firmware=DfuBoard.fromFirmwareName(filename);
            if(board!=null&&firmware!=null&&firmware!=board)
                throw new IOException("Файл для "+firmware.title+". На диске загрузчика определена "+board.title+".");
            String chipset=(model+" "+boardId).toLowerCase(Locale.ROOT);
            Ota.need(board!=null||chipset.contains("nrf52840")||chipset.contains("n5262")||
                chipset.contains("nrf5262"),"Диск не определён как UF2-загрузчик nRF52840.");
            Ota.need(softDevice.isEmpty()||softDevice.matches("(?i)S140\\b.*\\b6\\.1\\.1\\b.*"),
                "На плате "+softDevice+". Для этой прошивки нужен S140 6.1.1.");
        }
    }

    private Uf2Drive(Uri tree,Info info){
        this.tree=tree;
        this.info=info;
    }

    static String rootId(Uri tree)throws IOException {
        Ota.need("content".equals(tree.getScheme())&&EXTERNAL_STORAGE.equals(tree.getAuthority())&&
            tree.getPathSegments().size()==2&&"tree".equals(tree.getPathSegments().get(0)),
            "Выберите сам USB-диск платы в системном меню.");
        String documentId=DocumentsContract.getTreeDocumentId(tree);
        // Public FAT volumes use their filesystem UUID followed by ':' for the root.
        Ota.need(documentId.matches("(?i)[0-9a-f]{4}-[0-9a-f]{4}:"),
            "Откройте корень съёмного USB-диска загрузчика.");
        return documentId;
    }

    static Info parseInfo(String text)throws IOException {
        String model="",boardId="",version="",softDevice="";
        for(String line:text.replace("\uFEFF","").split("\\r?\\n")){
            line=line.trim();
            if(line.regionMatches(true,0,"UF2 Bootloader",0,14)){
                String tail=line.substring(14).trim();
                version=tail.replaceFirst("^:\\s*","").split("\\s+",2)[0];
            }
            int separator=line.indexOf(':');
            if(separator<0)continue;
            String value=line.substring(separator+1).trim();
            switch(line.substring(0,separator).trim().toLowerCase(Locale.ROOT)){
                case "model":model=value;break;
                case "board-id":case "board id":boardId=value;break;
                case "softdevice":softDevice=value;break;
            }
        }
        Ota.need(!version.isEmpty()&&(!model.isEmpty()||!boardId.isEmpty()),
            "В INFO_UF2.TXT не найдены модель платы и версия загрузчика.");
        return new Info(model,boardId,version,softDevice);
    }

    static Uf2Drive inspect(ContentResolver resolver,Uri tree)throws IOException {
        String id=rootId(tree);
        Uri children=DocumentsContract.buildChildDocumentsUriUsingTree(tree,id);
        Uri infoFile=null;
        try(Cursor cursor=resolver.query(children,new String[]{Document.COLUMN_DOCUMENT_ID,
            Document.COLUMN_DISPLAY_NAME},null,null,null)){
            while(cursor!=null&&cursor.moveToNext()){
                if(INFO_FILE.equalsIgnoreCase(cursor.getString(1))){
                    infoFile=DocumentsContract.buildDocumentUriUsingTree(tree,cursor.getString(0));
                    break;
                }
            }
        }
        Ota.need(infoFile!=null,"На выбранном диске нет INFO_UF2.TXT. Выберите диск загрузчика платы.");
        try(InputStream in=resolver.openInputStream(infoFile);ByteArrayOutputStream bytes=new ByteArrayOutputStream()){
            Ota.need(in!=null,"Не удалось прочитать INFO_UF2.TXT.");
            byte[] buffer=new byte[512];
            int count;
            while((count=in.read(buffer))!=-1){
                Ota.need(bytes.size()+count<=4096,"Некорректный размер INFO_UF2.TXT.");
                bytes.write(buffer,0,count);
            }
            return new Uf2Drive(tree,parseInfo(new String(bytes.toByteArray(),StandardCharsets.UTF_8)));
        }
    }

    void write(ContentResolver resolver,String name,byte[] firmware,Ota.Log log)throws Exception {
        write(resolver,name,firmware,log,Ota.Progress.NONE);
    }
    void write(ContentResolver resolver,String name,byte[] firmware,Ota.Log log,Ota.Progress progress)throws Exception {
        Info current=inspect(resolver,tree).info;
        Ota.need(info.sameAs(current),"Диск загрузчика изменился. Повторите проверку устройства.");
        current.checkFirmwareName(name);
        LegacyPackage.fromUf2(firmware);
        Uri directory=DocumentsContract.buildDocumentUriUsingTree(tree,rootId(tree));
        Uri output=DocumentsContract.createDocument(resolver,directory,"application/octet-stream","UPDATE.UF2");
        Ota.need(output!=null,"Не удалось создать файл на USB-диске.");
        ParcelFileDescriptor descriptor=resolver.openFileDescriptor(output,"w");
        Ota.need(descriptor!=null,"Не удалось открыть файл на USB-диске.");
        int sent=0;
        int lastPercent=0;
        progress.update("Запись UF2 на USB-диск",0,firmware.length);
        try(ParcelFileDescriptor.AutoCloseOutputStream out=new ParcelFileDescriptor.AutoCloseOutputStream(descriptor)){
            while(sent<firmware.length){
                int count=Math.min(16*1024,firmware.length-sent);
                out.write(firmware,sent,count);
                sent+=count;
                progress.update("Запись UF2 на USB-диск",sent,firmware.length);
                int percent=sent*100/firmware.length;
                if(percent/10>lastPercent){lastPercent=percent/10;log.show("UF2 передано "+percent+" %");}
            }
            progress.update("Подтверждение записи USB",sent,firmware.length);
            out.getFD().sync();
        }catch(IOException e){
            if(sent==firmware.length)
                throw new IOException("Диск не подтвердил запись. Проверьте устройство после перезагрузки.",e);
            throw new IOException("Передано "+sent/1024+" из "+firmware.length/1024+" КиБ. Проверьте USB-подключение.",e);
        }
    }
}
