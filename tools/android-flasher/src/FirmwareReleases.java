package ru.ommesh.otg;

import org.json.*;
import java.io.*;
import java.net.*;
import java.nio.charset.StandardCharsets;
import java.util.*;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

final class FirmwareReleases {
    static final String[] BOARDS={"", "heltec-v4", "heltec-v3", "xiao-c6",
        "t-echo", "promicro", "heltec-paper", "heltec-t114", "t3s3", "heltec-t096"};
    static final int REPEATER=1, COMPANION=2;
    static final int MAX_IMAGE=8*1024*1024;
    private static final int MAX_MCFW_ENVELOPE=54+31+8*96;
    private static final Pattern RELEASE_BUILD=Pattern.compile("-r([0-9]+)$");
    private static final Pattern DELTA_BASE=Pattern.compile("\\.from-r([0-9]+)\\.mcfd$");

    static final class Target {
        final String board, model;
        final int role, capacity;
        final long installed;
        final boolean uf2, deltaOnly, hasBaseImage;

        private Target(String board,String model,int role,int capacity,long installed,boolean uf2,
                       boolean deltaOnly,boolean hasBaseImage) {
            this.board=board;this.model=model;this.role=role;this.capacity=capacity;
            this.installed=installed;this.uf2=uf2;this.deltaOnly=deltaOnly;this.hasBaseImage=hasBaseImage;
        }

        static Target ota(Ota.Device device)throws IOException {
            String board=board(device.board);
            Ota.need(device.role==REPEATER||device.role==COMPANION,"Плата не сообщила роль прошивки.");
            long build=device.build&0xffffffffL;
            // T114 and T096 use in-place updates. Pro Micro switched to them in r142.
            boolean deltaOnly=device.board==7||device.board==9||(device.board==5&&build>=142);
            boolean hasBaseImage=false;
            if(device.runningSize>0&&device.hash!=null){
                for(byte value:device.hash){
                    if(value!=0){hasBaseImage=true;break;}
                }
            }
            return new Target(board,device.model.isEmpty()?board:device.model,device.role,
                device.capacity,build,false,deltaOnly,hasBaseImage);
        }

        static Target uf2(DfuBoard board) {
            return new Target(board.releaseName,board.title,0,0,0,true,false,false);
        }

        String updateIssue(Asset asset) {
            if(uf2)return "";
            if(installed==0||asset.build==0)return "Номер сборки неизвестен";
            if(asset.build==installed)return "Эта сборка уже на плате";
            if(asset.build<installed)return "Старее установленной сборки";
            if(asset.format==FirmwareFormat.MCFD)
                return hasBaseImage?"":"Плата не сообщила данные исходной прошивки";
            if(deltaOnly)return "Для обновления этой платы нужна дельта .mcfd";
            if(asset.size-MAX_MCFW_ENVELOPE>capacity)return "Образ больше слота обновления";
            return "";
        }
    }

    static final class Asset {
        final String name, url, digest, tag;
        final FirmwareFormat format;
        final long build, base;
        final int size, role;
        final boolean prerelease;

        Asset(JSONObject metadata,String name,String tag,FirmwareFormat format,int role,long base,
              boolean prerelease)throws JSONException {
            this.name=name;this.tag=tag;this.format=format;this.role=role;this.base=base;this.prerelease=prerelease;
            url=metadata.getString("browser_download_url");
            digest=metadata.optString("digest","");
            size=metadata.getInt("size");
            build=buildNumber(RELEASE_BUILD,tag);
        }
    }

    static String board(int id)throws IOException {
        Ota.need(id>0 && id<BOARDS.length,"Плата не сообщила поддерживаемую модель. Проверьте её прошивку.");
        return BOARDS[id];
    }

    static String roleTitle(int role) {
        return role==REPEATER?"Репитер":"Компаньон";
    }

    private static int fileRole(String name,String board) {
        if(name.startsWith(board+"-repeater-")||name.startsWith(board+"-repeater."))return REPEATER;
        if(name.startsWith(board+"-companion-")||name.startsWith(board+"-companion."))return COMPANION;
        return 0;
    }

    private static long buildNumber(Pattern pattern,String text) {
        Matcher matcher=pattern.matcher(text);
        if(!matcher.find())return 0;
        try{
            long number=Long.parseLong(matcher.group(1));
            return number<=0xffffffffL?number:0;
        }catch(NumberFormatException ignored){return 0;}
    }

    static List<Asset> parse(String json,Target target)throws JSONException {
        JSONArray releases=new JSONArray(json);
        List<Asset> result=new ArrayList<>();
        for(int i=0;i<releases.length();i++){
            JSONObject release=releases.getJSONObject(i);
            if(release.optBoolean("draft"))continue;
            String tag=release.getString("tag_name");
            boolean prerelease=release.optBoolean("prerelease");
            JSONArray assets=release.getJSONArray("assets");
            for(int j=0;j<assets.length();j++){
                JSONObject metadata=assets.getJSONObject(j);
                String name=metadata.getString("name");
                int role=fileRole(name,target.board);
                if(role==0||(!target.uf2&&role!=target.role))continue;
                FirmwareFormat format=FirmwareFormat.fromName(name);
                long base=buildNumber(DELTA_BASE,name);
                if(target.uf2){
                    String expected=target.board+(role==REPEATER?"-repeater.uf2":"-companion.uf2");
                    if(!name.equals(expected))continue;
                }else if(format!=FirmwareFormat.MCFW&&
                    (format!=FirmwareFormat.MCFD||base==0||base!=target.installed))continue;
                Asset asset=new Asset(metadata,name,tag,format,role,base,prerelease);
                if(asset.size>0&&asset.size<=MAX_IMAGE)result.add(asset);
            }
        }
        Collections.sort(result,(first,second)->{
            int newest=Long.compare(second.build,first.build);
            if(newest!=0)return newest;
            return Boolean.compare(second.format==FirmwareFormat.MCFD,first.format==FirmwareFormat.MCFD);
        });
        return result;
    }

    static Asset recommended(List<Asset> updates) {
        for(Asset asset:updates)if(!asset.prerelease)return asset;
        return updates.isEmpty()?null:updates.get(0);
    }

    static List<Asset> load(Target target)throws Exception {
        String url="https://api.github.com/repos/cm4ker/meshcore-rs/releases?per_page=20";
        return parse(new String(fetch(url,8*1024*1024),StandardCharsets.UTF_8),target);
    }
    static byte[] fetch(String address,int limit)throws IOException {
        URL url=new URL(address);
        for(int redirect=0;redirect<6;redirect++){
            Ota.need("https".equals(url.getProtocol()),"Загрузка требует HTTPS.");
            HttpURLConnection c=(HttpURLConnection)url.openConnection();
            c.setConnectTimeout(15000);c.setReadTimeout(30000);c.setInstanceFollowRedirects(false);
            c.setRequestProperty("User-Agent","ota-app-Android");
            try{
                int status=c.getResponseCode();
                if(status==301||status==302||status==303||status==307||status==308){
                    String location=c.getHeaderField("Location");
                    Ota.need(location!=null,"Пустой адрес перенаправления.");
                    url=new URL(url,location);continue;
                }
                Ota.need(status==200,status==403||status==429?
                    "GitHub ограничил запросы. Попробуйте позже.":"Ошибка загрузки HTTP "+status);
                Ota.need(c.getContentLength()<=limit,"Файл превышает допустимый размер.");
                try(InputStream in=c.getInputStream();ByteArrayOutputStream out=new ByteArrayOutputStream()){
                    byte[] buffer=new byte[16384];int n;
                    while((n=in.read(buffer))!=-1){
                        Ota.need(out.size()+n<=limit,"Файл превышает допустимый размер.");
                        out.write(buffer,0,n);
                    }
                    return out.toByteArray();
                }
            }finally{c.disconnect();}
        }
        throw new IOException("Слишком много перенаправлений.");
    }
    static File download(Asset asset,File directory,Target target)throws Exception {
        Ota.need(asset.name.matches("[a-zA-Z0-9._-]+") &&
            asset.url.startsWith("https://github.com/cm4ker/meshcore-rs/releases/download/"),"Неверный адрес файла выпуска.");
        Ota.need(asset.size>0 && asset.size<=MAX_IMAGE,"Недопустимый размер файла выпуска.");
        byte[] bytes=fetch(asset.url,MAX_IMAGE);
        Ota.need(bytes.length==asset.size,"Файл загружен не полностью.");
        if(asset.digest.startsWith("sha256:")){
            StringBuilder hash=new StringBuilder();
            for(byte b:Ota.digest(bytes))hash.append(String.format(Locale.ROOT,"%02x",b&255));
            Ota.need(asset.digest.substring(7).equalsIgnoreCase(hash.toString()),"Контрольная сумма загрузки не совпадает.");
        }
        if(target.uf2)LegacyPackage.fromUf2(bytes);
        else{
            Ota.need(bytes.length>=16 && bytes[0]=='M' && bytes[1]=='C' && bytes[2]=='F' &&
                (bytes[3]=='W'||bytes[3]=='D'),"Неверный формат прошивки в выпуске.");
            Ota.need(target.board.equals(board(bytes[5]&255)) && (bytes[6]&255)==asset.role,
                "Файл предназначен для другой платы или роли.");
        }
        Ota.need(directory.isDirectory()||directory.mkdirs(),"Не удалось создать папку загрузок.");
        File folder=new File(directory,UUID.randomUUID().toString());
        Ota.need(folder.mkdir(),"Не удалось создать папку выпуска.");
        File downloaded=new File(folder,asset.name);
        try{
            try(OutputStream out=new FileOutputStream(downloaded)){out.write(bytes);}
            return downloaded;
        }catch(Exception e){downloaded.delete();folder.delete();throw e;}
    }
}
