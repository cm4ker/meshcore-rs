package ru.ommesh.otg;

import android.content.Intent;
import android.webkit.MimeTypeMap;
import java.util.LinkedHashSet;
import java.util.Locale;
import java.util.Set;

enum FirmwareFormat {
    MCFW, MCFD, BIN, UF2;

    static FirmwareFormat fromName(String name) {
        if(name==null)return null;
        int dot=name.lastIndexOf('.');
        if(dot<0)return null;
        String extension=name.substring(dot+1).toUpperCase(Locale.ROOT);
        for(FirmwareFormat format:values())if(format.name().equals(extension))return format;
        return null;
    }

    static boolean allowed(String name,int mode) {
        FirmwareFormat format=fromName(name);
        return format!=null&&format.supportsMode(mode);
    }

    boolean supportsMode(int mode) {
        switch(mode){
            case 0: return true;
            case 1: return this==MCFW||this==MCFD;
            case 2: return this!=UF2;
            case 3: return this==UF2;
            default: return false;
        }
    }

    static String extensions(int mode) {
        if(mode==3)return ".uf2";
        if(mode==1)return ".mcfw или .mcfd";
        return mode==0?".mcfw, .mcfd, .bin и .uf2":".mcfw, .mcfd и .bin";
    }

    static Intent picker(int mode) {
        // DocumentsUI filters by MIME. Unknown firmware extensions normally use octet-stream.
        Set<String> mime=new LinkedHashSet<>();
        mime.add("application/octet-stream");
        for(FirmwareFormat format:values()){
            String ext=format.name().toLowerCase(Locale.ROOT);
            if(!format.supportsMode(mode))continue;
            String mapped=MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext);
            if(mapped!=null)mime.add(mapped);
            mime.add("application/x-"+ext);
        }
        return new Intent(Intent.ACTION_OPEN_DOCUMENT).setType("application/octet-stream")
            .addCategory(Intent.CATEGORY_OPENABLE)
            .putExtra(Intent.EXTRA_MIME_TYPES,mime.toArray(new String[0]));
    }
}
