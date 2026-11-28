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
        "t-echo", "promicro", "heltec-paper", "heltec-t114", "t3s3", "heltec-t096", "heltec-mesh-pocket"};
    static final String API="https://api.github.com/repos/cm4ker/meshcore-rs/releases?per_page=20";
    static final int REPEATER=1, COMPANION=2;
    static final int MAX_IMAGE=8*1024*1024;
    private static final int MAX_MCFW_ENVELOPE=54+31+8*96;
    static final Pattern RELEASE_BUILD=Pattern.compile("-r([0-9]+)$");
    static final Pattern DELTA_BASE=Pattern.compile("\\.from-r([0-9]+)\\.mcfd$");
    static final Pattern FILENAME=Pattern.compile("[A-Za-z0-9][A-Za-z0-9._-]{0,254}");
    static final Pattern RELEASE_TAG=Pattern.compile("[A-Za-z0-9](?:[A-Za-z0-9._-]{0,94}[A-Za-z0-9])?");

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
            return updateIssue(asset.format,asset.size,asset.build);
        }

        String updateIssue(FirmwareFormat format,int size,long build) {
            if(uf2)return "";
            if(installed==0||build==0)return "Номер сборки неизвестен";
            if(build==installed)return "Эта сборка уже на плате";
            if(build<installed)return "Старее установленной сборки";
            if(format==FirmwareFormat.MCFD)
                return hasBaseImage?"":"Плата не сообщила данные исходной прошивки";
            if(deltaOnly)return "Для обновления этой платы нужна дельта .mcfd";
            if(size-MAX_MCFW_ENVELOPE>capacity)return "Образ больше слота обновления";
            return "";
        }
    }

    static final class Asset {
        final String name, url, digest, tag;
        final FirmwareFormat format;
        final long build, base;
        final int size, board, role;
        final boolean prerelease;

        Asset(JSONObject metadata,String name,String tag,FirmwareFormat format,int board,int role,long base,
              boolean prerelease)throws JSONException {
            this.name=name;this.tag=tag;this.format=format;this.board=board;this.role=role;this.base=base;this.prerelease=prerelease;
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

    static int fileRole(String name,String board) {
        if(name.startsWith(board+"-repeater-")||name.startsWith(board+"-repeater."))return REPEATER;
        if(name.startsWith(board+"-companion-")||name.startsWith(board+"-companion."))return COMPANION;
        return 0;
    }

    static long buildNumber(Pattern pattern,String text) {
        Matcher matcher=pattern.matcher(text);
        if(!matcher.find())return 0;
        try{
            long number=Long.parseLong(matcher.group(1));
            return number<=0xffffffffL?number:0;
        }catch(NumberFormatException ignored){return 0;}
    }

    static int fileBoard(String name){
        if(!FILENAME.matcher(name).matches())return 0;
        for(int id=1;id<BOARDS.length;id++)if(fileRole(name,BOARDS[id])!=0)return id;
        return 0;
    }

    static boolean bundledUf2(String name,int board,int role){
        return (board==4||board==5||board==7||board==9)
            && name.equals(BOARDS[board]+(role==REPEATER?"-repeater.uf2":"-companion.uf2"));
    }

    static Asset releaseAsset(JSONObject metadata,String tag,boolean prerelease)throws JSONException {
        String name=metadata.getString("name");
        int board=fileBoard(name);
        if(board==0)return null;
        int role=fileRole(name,BOARDS[board]);
        FirmwareFormat format=FirmwareFormat.fromName(name);
        long base=buildNumber(DELTA_BASE,name);
        if(format==FirmwareFormat.UF2&&!bundledUf2(name,board,role))return null;
        if(format!=FirmwareFormat.MCFW&&format!=FirmwareFormat.UF2&&
                (format!=FirmwareFormat.MCFD||base==0))return null;
        Asset asset=new Asset(metadata,name,tag,format,board,role,base,prerelease);
        return asset.size>0&&asset.size<=MAX_IMAGE?asset:null;
    }

    static List<Asset> parse(String json,Target target)throws JSONException {
        JSONArray releases=new JSONArray(json);
        List<Asset> result=new ArrayList<>();
        for(int i=0;i<releases.length();i++){
            JSONObject release=releases.getJSONObject(i);
            if(release.optBoolean("draft"))continue;
            String tag=release.getString("tag_name");
            if(!RELEASE_TAG.matcher(tag).matches())continue;
            boolean prerelease=release.optBoolean("prerelease");
            JSONArray assets=release.getJSONArray("assets");
            for(int j=0;j<assets.length();j++){
                Asset asset=releaseAsset(assets.getJSONObject(j),tag,prerelease);
                if(asset==null||!BOARDS[asset.board].equals(target.board))continue;
                if(target.uf2?asset.format==FirmwareFormat.UF2:
                        asset.role==target.role&&(asset.format==FirmwareFormat.MCFW||
                        asset.format==FirmwareFormat.MCFD&&asset.base==target.installed))result.add(asset);
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
        return parse(new String(fetch(API,8*1024*1024),StandardCharsets.UTF_8),target);
    }
    static byte[] fetch(String address,int limit)throws IOException {
        return fetch(address,limit,0,Ota.Progress.NONE);
    }
    static byte[] fetch(String address,int limit,int expectedSize,Ota.Progress progress)throws IOException {
        URL url=new URL(address);
        for(int redirect=0;redirect<6;redirect++){
            String host=url.getHost();
            Ota.need("https".equals(url.getProtocol())&&("api.github.com".equals(host)||"github.com".equals(host)||
                "release-assets.githubusercontent.com".equals(host)||"objects.githubusercontent.com".equals(host)),
                "GitHub перенаправил загрузку на неожиданный адрес.");
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
                        if(expectedSize>0)progress.update("Загрузка прошивки",out.size(),expectedSize);
                    }
                    return out.toByteArray();
                }
            }finally{c.disconnect();}
        }
        throw new IOException("Слишком много перенаправлений.");
    }
    static boolean validAssetUrl(Asset asset){
        return RELEASE_TAG.matcher(asset.tag).matches()&&FILENAME.matcher(asset.name).matches()&&
            asset.url.equals("https://github.com/cm4ker/meshcore-rs/releases/download/"+asset.tag+"/"+asset.name);
    }

    static byte[] downloadBytes(Asset asset,Ota.Progress progress)throws Exception {
        Ota.need(validAssetUrl(asset),"Неверный адрес файла выпуска.");
        Ota.need(asset.size>0 && asset.size<=MAX_IMAGE,"Недопустимый размер файла выпуска.");
        byte[] bytes=fetch(asset.url,MAX_IMAGE,asset.size,progress);
        Ota.need(bytes.length==asset.size,"Файл загружен не полностью.");
        progress.update("Проверка загруженного файла",bytes.length,asset.size);
        if(!asset.digest.isEmpty())
            Ota.need(asset.digest.equalsIgnoreCase("sha256:"+hex(Ota.digest(bytes))),"Контрольная сумма загрузки не совпадает.");
        validateFirmware(asset.format,asset.board,asset.role,asset.build,bytes);
        return bytes;
    }

    static String hex(byte[] bytes){
        StringBuilder hash=new StringBuilder(bytes.length*2);
        for(byte b:bytes){
            hash.append(Character.forDigit((b>>4)&15,16)).append(Character.forDigit(b&15,16));
        }
        return hash.toString();
    }

    static int envelopeEnd(byte[] data)throws IOException{
        Ota.need(data.length>=54&&data[0]=='M'&&data[1]=='C'&&data[2]=='F'&&data[3]=='W'&&data[4]==1&&(data[52]&255)<=31,
            "Неверный заголовок прошивки.");
        int index=53+(data[52]&255);
        Ota.need(index<data.length&&(data[index]&255)>0&&(data[index]&255)<=8,"Неверный конверт выпуска.");
        int end=index+1+96*(data[index]&255);
        Ota.need(end<data.length,"Неверная длина конверта выпуска.");
        return end;
    }

    static void validateFirmware(FirmwareFormat format,int board,int role,long build,byte[] data)throws Exception{
        if(format==FirmwareFormat.UF2){
            LegacyPackage.fromUf2(data);
            return;
        }
        int end=envelopeEnd(data);
        Ota.need((data[5]&255)==board&&(data[6]&255)==role&&(Ota.i32(data,8)&0xffffffffL)==build,
            "Сборка, модель платы или роль файла не соответствуют выпуску.");
        int imageSize=Ota.i32(data,12);
        Ota.need(imageSize>0,"Неверный размер образа.");
        if(format==FirmwareFormat.MCFD)
            Ota.need(data.length>=end+38&&Ota.i32(data,end)>0,"Неверная длина дельты MCFD.");
        else
            Ota.need(data.length-end==imageSize&&Arrays.equals(Arrays.copyOfRange(data,16,48),
                Ota.digest(Arrays.copyOfRange(data,end,data.length))),"Контрольная сумма образа не совпадает.");
    }
}
