package ru.ommesh.otg;

import android.os.Build;
import android.system.ErrnoException;
import android.system.Os;
import org.json.*;
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.StandardCopyOption;
import java.util.*;

/** Local copies of GitHub release assets, usable independently of the catalog. */
final class FirmwareLibrary {
    static final String[] MODELS={"Все платы","Heltec V4","Heltec V3","Seeed XIAO ESP32-C6",
        "LilyGO T-Echo","Pro Micro (FakeTec)","Heltec Wireless Paper","Heltec T114",
        "LilyGO T3-S3","Heltec T096","Heltec Mesh Pocket"};

    static final class Version {
        final String tag;
        final long build;
        final boolean prerelease;
        final List<FirmwareReleases.Asset> assets;

        Version(String tag,boolean prerelease,List<FirmwareReleases.Asset> assets) {
            this.tag=tag;this.prerelease=prerelease;this.assets=assets;
            build=FirmwareReleases.buildNumber(FirmwareReleases.RELEASE_BUILD,tag);
        }

        List<FirmwareReleases.Asset> select(int board,int role) {
            List<FirmwareReleases.Asset> result=new ArrayList<>();
            for(FirmwareReleases.Asset asset:assets)
                if((board==0||board==asset.board)&&(role==0||role==asset.role))result.add(asset);
            return result;
        }
    }

    static final class Entry {
        final File file;
        final String tag,digest,baseHash;
        final FirmwareFormat format;
        final long build,base;
        final int board,role,size,imageSize,baseSize;
        final boolean prerelease;
        String issue="";
        boolean enabled=true,recommended;

        Entry(File file,JSONObject data)throws JSONException {
            this.file=file;
            format=FirmwareFormat.fromName(file.getName());
            tag=data.getString("tag");digest=data.getString("digest");baseHash=data.optString("base_hash");
            board=data.getInt("board");role=data.getInt("role");size=data.getInt("size");
            imageSize=data.optInt("image_size");baseSize=data.optInt("base_size");
            build=data.getLong("build");base=data.getLong("base");prerelease=data.getBoolean("prerelease");
        }

        String key(){return file.getName()+"|"+digest+"|"+build;}

        String label(){
            String prefix=recommended?"Рекомендуем · ":"";
            String version=build>0?"r"+build:tag;
            String kind;
            if(format==FirmwareFormat.MCFD)kind="Дельта .mcfd · r"+base+" → "+version;
            else if(format==FirmwareFormat.MCFW)kind="Полная .mcfw · "+version;
            else kind="UF2 · "+version;
            return prefix+kind+"\n"+MODELS[board]+" · "+FirmwareReleases.roleTitle(role);
        }
    }

    static final class Saved {
        final List<Entry> files;
        final int total;
        Saved(List<Entry> files,int total){this.files=files;this.total=total;}
    }

    static final class Result {
        final File file;
        final boolean downloaded;
        Result(File file,boolean downloaded){this.file=file;this.downloaded=downloaded;}
    }

    static List<Version> versions()throws Exception{
        return parse(new String(FirmwareReleases.fetch(FirmwareReleases.API,FirmwareReleases.MAX_IMAGE),StandardCharsets.UTF_8));
    }

    static List<Version> parse(String json)throws JSONException{
        JSONArray releases=new JSONArray(json);
        List<Version> result=new ArrayList<>();
        for(int i=0;i<releases.length();i++){
            JSONObject release=releases.getJSONObject(i);
            String tag=release.getString("tag_name");
            if(release.optBoolean("draft")||!FirmwareReleases.RELEASE_TAG.matcher(tag).matches())continue;
            boolean prerelease=release.optBoolean("prerelease");
            JSONArray files=release.getJSONArray("assets");
            List<FirmwareReleases.Asset> assets=new ArrayList<>();
            for(int j=0;j<files.length();j++){
                FirmwareReleases.Asset asset=FirmwareReleases.releaseAsset(files.getJSONObject(j),tag,prerelease);
                if(asset!=null&&(asset.format==FirmwareFormat.UF2||asset.build!=0)&&FirmwareReleases.validAssetUrl(asset))
                    assets.add(asset);
            }
            if(!assets.isEmpty())result.add(new Version(tag,prerelease,assets));
        }
        Collections.sort(result,(left,right)->{
            int stage=Boolean.compare(left.prerelease,right.prerelease);
            return stage!=0?stage:Long.compare(right.build,left.build);
        });
        return result;
    }

    static synchronized Result store(FirmwareReleases.Asset asset,File directory,Ota.Progress progress)throws Exception{
        Ota.need(FirmwareReleases.validAssetUrl(asset),"Неверный адрес файла выпуска.");
        File folder=new File(new File(directory,"library"),"release-"+asset.tag);
        File file=new File(folder,asset.name);
        File metadata=new File(folder,asset.name+".json");
        Entry saved=readEntry(metadata);
        if(saved!=null&&saved.size==asset.size&&(asset.digest.isEmpty()||asset.digest.equalsIgnoreCase(saved.digest))){
            try{return new Result(verify(saved,directory),false);}
            catch(IOException ignored){/* Repair a file no longer matching its checksum. */}
        }
        byte[] bytes=FirmwareReleases.downloadBytes(asset,progress);
        JSONObject record=record(asset.tag,asset.prerelease,asset.board,asset.role,asset.build,asset.base,asset.format,bytes);
        Ota.need(folder.isDirectory()||folder.mkdirs(),"Не удалось создать папку выпуска.");
        write(file,bytes);
        write(metadata,record.toString().getBytes(StandardCharsets.UTF_8));
        Entry stored=readEntry(metadata);
        Ota.need(stored!=null,"Не удалось сохранить прошивку в библиотеке.");
        return new Result(verify(stored,directory),true);
    }

    static int sync(List<FirmwareReleases.Asset> files,File directory,Ota.Progress progress)throws Exception{
        long length=0;
        for(FirmwareReleases.Asset file:files)length+=file.size;
        Ota.need(length>0&&length<=Integer.MAX_VALUE,"Недопустимый размер офлайн-набора.");
        int total=(int)length, done=0, downloaded=0, ready=0;
        progress.update("Загрузка офлайн-набора",0,total);
        for(FirmwareReleases.Asset asset:files){
            int offset=done, bytes=total;
            String stage="Загрузка файла "+(ready+1)+" из "+files.size();
            try{
                Result result=store(asset,directory, (label,count,ignored)->progress.update(stage,offset+count,bytes));
                if(result.downloaded)downloaded++;
            }catch(Exception error){
                throw new IOException("В наборе сохранено "+ready+" из "+files.size()+" файлов. "+
                    asset.name+". "+error.getMessage(),error);
            }
            done+=asset.size;
            ready++;
            progress.update(stage,done,total);
        }
        return downloaded;
    }

    static Saved saved(File directory,int mode,boolean uf2Only,int board,Ota.Device device)throws Exception{
        FirmwareReleases.Target target=device!=null&&device.board>0&&device.board<FirmwareReleases.BOARDS.length&&
            (device.role==FirmwareReleases.REPEATER||device.role==FirmwareReleases.COMPANION)?
            FirmwareReleases.Target.ota(device):null;
        int selectedBoard=target==null?board:device.board;
        Set<String> all=new HashSet<>();
        Map<String,Entry> selected=new LinkedHashMap<>();
        for(Entry item:entries(directory)){
            String key=item.key();
            all.add(key);
            if(!item.format.supportsMode(mode)||(uf2Only&&item.format!=FirmwareFormat.UF2)||
                    (selectedBoard!=0&&item.board!=selectedBoard)||(target!=null&&item.format!=FirmwareFormat.UF2&&item.role!=target.role))
                continue;
            if(selected.containsKey(key))continue;
            if(target!=null&&item.format!=FirmwareFormat.UF2)item.issue=issue(item,target,device,mode);
            boolean esp=item.board==1||item.board==2||item.board==3||item.board==6||item.board==8;
            item.enabled=item.issue.isEmpty()||mode==0&&item.format==FirmwareFormat.MCFW&&esp;
            selected.put(key,item);
        }
        List<Entry> files=new ArrayList<>(selected.values());
        Collections.sort(files,FirmwareLibrary::compareEntries);
        if(target!=null){
            for(Entry entry:files){
                if(entry.issue.isEmpty()&&entry.format!=FirmwareFormat.UF2){entry.recommended=true;break;}
            }
        }
        return new Saved(files,all.size());
    }

    private static int compareEntries(Entry first,Entry second){
        int order=Boolean.compare(!first.issue.isEmpty(),!second.issue.isEmpty());
        if(order==0)order=Boolean.compare(first.prerelease,second.prerelease);
        if(order==0)order=Long.compare(second.build,first.build);
        if(order==0)order=Boolean.compare(first.format!=FirmwareFormat.MCFD,second.format!=FirmwareFormat.MCFD);
        if(order==0)order=MODELS[first.board].compareTo(MODELS[second.board]);
        return order==0?Integer.compare(first.role,second.role):order;
    }

    private static String issue(Entry file,FirmwareReleases.Target target,Ota.Device device,int mode){
        if(device.capacity<=0)return "На плате нет слота обновления";
        if(mode==1&&device.roots==0)return "Для Wi-Fi / TCP нужен доверенный ключ выпуска";
        String issue=target.updateIssue(file.format,file.size,file.build);
        if(!issue.isEmpty())return issue;
        if(file.format==FirmwareFormat.MCFD&&(file.base!=target.installed||file.baseSize!=device.runningSize||
                !file.baseHash.equals(FirmwareReleases.hex(Arrays.copyOf(device.hash,8)))))
            return "Дельта рассчитана на другую исходную прошивку";
        if(file.format==FirmwareFormat.MCFW&&file.imageSize>device.capacity)return "Образ больше слота обновления";
        return "";
    }

    static File verify(Entry entry,File directory)throws Exception{
        File file=entry.file.getCanonicalFile();
        Ota.need(file.getPath().startsWith(directory.getCanonicalPath()+File.separator)&&
                file.isFile()&&file.length()==entry.size&&entry.size>0&&entry.size<=FirmwareReleases.MAX_IMAGE,
            "Сохранённый файл повреждён или удалён. Скачайте его заново.");
        Ota.need(entry.digest.equals("sha256:"+FirmwareReleases.hex(Ota.digest(read(file,FirmwareReleases.MAX_IMAGE)))),
            "Сохранённый файл повреждён. Скачайте его заново.");
        return file;
    }

    private static List<Entry> entries(File directory){
        List<Entry> found=new ArrayList<>();
        for(File folder:children(new File(directory,"library"))){
            if(!folder.isDirectory()||!folder.getName().startsWith("release-"))continue;
            for(File metadata:children(folder)){
                if(!metadata.getName().endsWith(".json"))continue;
                Entry entry=readEntry(metadata);
                if(entry!=null)found.add(entry);
            }
        }
        for(File folder:children(directory)){
            if(!folder.isDirectory()||!folder.getName().matches("[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}"))continue;
            for(File file:children(folder)){
                try{
                    Entry entry=legacyEntry(file);
                    if(entry!=null)found.add(entry);
                }catch(Exception ignored){/* Skip a previous download that is no longer valid. */}
            }
        }
        return found;
    }

    private static Entry legacyEntry(File file)throws Exception{
        String name=file.getName();
        int board=FirmwareReleases.fileBoard(name);
        FirmwareFormat format=FirmwareFormat.fromName(name);
        if(board==0||format==null||format==FirmwareFormat.BIN||!file.isFile()||
            file.length()<=0||file.length()>FirmwareReleases.MAX_IMAGE)return null;
        int role=FirmwareReleases.fileRole(name,FirmwareReleases.BOARDS[board]);
        if(format==FirmwareFormat.UF2&&!FirmwareReleases.bundledUf2(name,board,role))return null;
        long base=FirmwareReleases.buildNumber(FirmwareReleases.DELTA_BASE,name);
        if(format==FirmwareFormat.MCFD&&base==0)return null;
        byte[] data=read(file,FirmwareReleases.MAX_IMAGE);
        long build=format==FirmwareFormat.UF2||data.length<12?0:Ota.i32(data,8)&0xffffffffL;
        FirmwareReleases.validateFirmware(format,board,role,build,data);
        return new Entry(file,record(build==0?"Ранее скачано":"r"+build,false,board,role,build,base,format,data));
    }

    private static Entry readEntry(File metadata){
        try{
            if(!metadata.isFile()||metadata.length()>8192)return null;
            String leaf=metadata.getName();
            File file=new File(metadata.getParentFile(),leaf.substring(0,leaf.length()-5));
            int board=FirmwareReleases.fileBoard(file.getName());
            if(board==0||!file.isFile())return null;
            JSONObject data=new JSONObject(new String(read(metadata,8192),StandardCharsets.UTF_8));
            Entry entry=new Entry(file,data);
            if(entry.size<=0||entry.size>FirmwareReleases.MAX_IMAGE||file.length()!=entry.size||
                !entry.digest.matches("sha256:[0-9a-f]{64}")||entry.board!=board||
                entry.role!=FirmwareReleases.fileRole(file.getName(),FirmwareReleases.BOARDS[board])||
                entry.format==null||entry.format==FirmwareFormat.BIN)return null;
            return entry;
        }catch(IOException|JSONException ignored){return null;}
    }

    private static JSONObject record(String tag,boolean prerelease,int board,int role,long build,long base,
                                     FirmwareFormat format,byte[] data)throws Exception{
        JSONObject record=new JSONObject().put("tag",tag).put("prerelease",prerelease).put("board",board)
            .put("role",role).put("build",build).put("base",base).put("size",data.length)
            .put("digest","sha256:"+FirmwareReleases.hex(Ota.digest(data)));
        if(format!=FirmwareFormat.UF2)record.put("image_size",Ota.i32(data,12));
        if(format==FirmwareFormat.MCFD){
            int offset=FirmwareReleases.envelopeEnd(data);
            record.put("base_size",Ota.i32(data,offset)).put("base_hash",
                FirmwareReleases.hex(Arrays.copyOfRange(data,offset+4,offset+12)));
        }
        return record;
    }

    private static File[] children(File folder){
        File[] entries=folder.listFiles();
        return entries==null?new File[0]:entries;
    }

    private static byte[] read(File file,int limit)throws IOException{
        try(InputStream input=new FileInputStream(file);ByteArrayOutputStream bytes=new ByteArrayOutputStream()){
            byte[] buffer=new byte[16384];
            int count;
            while((count=input.read(buffer))!=-1){
                Ota.need(bytes.size()+count<=limit,"Сохранённый файл превышает допустимый размер.");
                bytes.write(buffer,0,count);
            }
            return bytes.toByteArray();
        }
    }

    private static void write(File file,byte[] bytes)throws IOException{
        File temporary=File.createTempFile("firmware-",".tmp",file.getParentFile());
        try{
            try(FileOutputStream output=new FileOutputStream(temporary)){
                output.write(bytes);
                output.getFD().sync();
            }
            if(Build.VERSION.SDK_INT>=26)
                Files.move(temporary.toPath(),file.toPath(),StandardCopyOption.ATOMIC_MOVE,StandardCopyOption.REPLACE_EXISTING);
            else
                Os.rename(temporary.getPath(),file.getPath());
        }catch(ErrnoException error){
            throw new IOException("Не удалось сохранить "+file.getName()+".",error);
        }finally{
            temporary.delete();
        }
    }
}
