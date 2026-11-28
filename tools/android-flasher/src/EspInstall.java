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
    static final int PARTITION_TABLE = 0x8000;
    static final int APP_ADDRESS = 0x10000;
    static final int SECTOR_SIZE = 4096;
    private static final int APP_DESCRIPTOR = 0xabcd5432;

    private enum Board {
        HELTEC_V4(1, "heltec-v4", "Heltec V4", 9),
        HELTEC_V3(2, "heltec-v3", "Heltec V3", 9),
        XIAO_C6(3, "xiao-c6", "Seeed XIAO ESP32-C6", 13),
        HELTEC_PAPER(6, "heltec-paper", "Heltec Wireless Paper", 9),
        T3S3(8, "t3s3", "LilyGO T3-S3", 9);

        final int id, chip;
        final String slug, title;

        Board(int id, String slug, String title, int chip) {
            this.id = id;
            this.slug = slug;
            this.title = title;
            this.chip = chip;
        }

        static Board fromId(int id) throws IOException {
            for (Board board : values()) if (board.id == id) return board;
            throw new IOException("Эта ESP-плата не поддерживается загрузчиком.");
        }
    }

    private static final class BootSlot {
        final long sequence;
        final int sector;

        BootSlot(long sequence, int sector) {
            this.sequence = sequence;
            this.sector = sector;
        }

        int inactive() { return 1 - (int) ((sequence - 1) & 1); }
    }

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
    private byte[] exchange(int op,byte[] body,int xor,int ms)throws IOException {
        slip(join(le(8).put((byte)0).put((byte)op).putShort((short)body.length).putInt(xor).array(),body));
        long deadline=System.nanoTime()+ms*1000000L;int tail=stub?2:4;
        for(;;){byte[] b=packet(deadline);
            if(b.length<8+tail||b[0]!=1||(b[1]&255)!=op)continue;
            need(u16(b,2)==b.length-8,"Длина пакета ESP не совпала.");
            need(b[b.length-tail]==0,"Загрузчик отклонил команду "+Integer.toHexString(op)+".");
            return b;
        }
    }
    byte[] command(int op, byte[] body, int xor, int ms) throws IOException {
        byte[] response = exchange(op, body, xor, ms);
        return Arrays.copyOfRange(response, 8, response.length - (stub ? 2 : 4));
    }

    int readRegister(int address) throws IOException {
        return i32(exchange(0x0a, ints(address), 0, 4000), 4);
    }

    void writeRegister(int address, int value) throws IOException {
        command(0x09, ints(address, value, -1, 0), 0, 4000);
    }

    int flashCapacity(int chip) throws IOException {
        command(0x0d, ints(0), 0, 4000);
        int base = chip == 9 ? 0x60002000 : 0x60003000;
        int user = base + 0x18, userCommand = base + 0x20;
        int readLength = base + 0x28, data = base + 0x58;
        int oldUser = readRegister(user), oldCommand = readRegister(userCommand), oldLength = readRegister(readLength);
        int id;
        try {
            // SPI Read Identification (0x9f) returns the three JEDEC ID bytes.
            writeRegister(readLength, 23);
            writeRegister(user, (1 << 31) | (1 << 28));
            writeRegister(userCommand, (7 << 28) | 0x9f);
            writeRegister(data, 0);
            int start = 1 << 18;
            writeRegister(base, start);
            int poll = 0;
            while (poll < 10 && (readRegister(base) & start) != 0) poll++;
            need(poll < 10, "Флеш-память ESP не ответила на запрос идентификатора.");
            id = readRegister(data);
        } finally {
            writeRegister(user, oldUser);
            writeRegister(userCommand, oldCommand);
            writeRegister(readLength, oldLength);
        }
        int code = (id >>> 16) & 255;
        // Some flash chips use alternative JEDEC codes for the same capacities.
        if (code >= 0x32 && code <= 0x3a) code -= 0x20;
        if (code >= 0x20 && code <= 0x22) code -= 6;
        need(code >= 0x12 && code <= 0x1c, "Не удалось определить размер флеш-памяти платы.");
        return 1 << code;
    }

    private void configureFlash(int capacity) throws IOException {
        command(0x0b, ints(0, capacity, 0x10000, SECTOR_SIZE, 256, 0xffff), 0, 4000);
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
        ByteBuffer blob=ByteBuffer.wrap(load(assets.open("esp/"+chipName+".stub"))).order(ByteOrder.LITTLE_ENDIAN);
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
    static boolean isMergedName(String name) {
        return name != null && name.toLowerCase(Locale.ROOT).endsWith("-merged.bin");
    }

    static boolean isDirectName(String name) {
        if (name == null) return false;
        String lower = name.toLowerCase(Locale.ROOT);
        return isMergedName(lower) || (lower.startsWith("heltec_v3_") && lower.endsWith(".bin"));
    }

    static String factoryDescription(byte[] header) throws IOException {
        need(header.length >= 7 && header[0] == 'M' && header[1] == 'C' && header[2] == 'F'
            && header[3] == 'W' && header[4] == 1, "Неверный заголовок MCFW.");
        Board board = Board.fromId(header[5] & 255);
        int role = header[6] & 255;
        need(role == 1 || role == 2, "В файле .mcfw указана неподдерживаемая роль.");
        return board.title + (role == 1 ? " · Репитер" : " · Компаньон");
    }

    static int embeddedImageEnd(byte[] bytes, int start, int limit, int chip, boolean app) throws Exception {
        need(start >= 0 && limit <= bytes.length && limit - start > 36 && (bytes[start] & 255) == 0xe9
            && u16(bytes, start + 12) == chip, "Повреждён образ ESP или указан другой чип.");
        if (app) need(i32(bytes, start + 32) == APP_DESCRIPTOR, "В образе отсутствует дескриптор приложения ESP.");
        int segments = bytes[start + 1] & 255;
        int hashFlag = bytes[start + 23] & 255;
        need(segments > 0 && segments <= 16 && hashFlag <= 1, "Неверный заголовок образа ESP.");
        int offset = start + 24;
        int checksum = 0xef;
        for (int i = 0; i < segments; i++) {
            need(offset + 8 <= limit, "Оборван сегмент ESP.");
            int length = i32(bytes, offset + 4);
            offset += 8;
            need(length >= 0 && length <= limit - offset, "Неверная длина сегмента ESP.");
            for (int j = 0; j < length; j++) checksum ^= bytes[offset++] & 255;
        }
        int checksumOffset = start + ((offset - start) | 15);
        int imageEnd = checksumOffset + 1 + hashFlag * 32;
        need(imageEnd <= limit && (bytes[checksumOffset] & 255) == checksum, "Ошибка контрольной суммы образа ESP.");
        if (hashFlag == 1) {
            MessageDigest sha = MessageDigest.getInstance("SHA-256");
            sha.update(bytes, start, checksumOffset + 1 - start);
            need(Arrays.equals(sha.digest(), Arrays.copyOfRange(bytes, checksumOffset + 1, imageEnd)),
                "Ошибка SHA-256 образа ESP.");
        }
        return imageEnd;
    }

    static byte[] validate(byte[] file,int board,int chip)throws Exception{
        byte[] app=file;
        if(file.length>=4&&file[0]=='M'&&file[1]=='C'&&file[2]=='F'&&file[3]=='W'){
            need(file.length>=54&&file[4]==1&&(file[5]&255)==board,"Файл MCFW другой модели или повреждён.");
            int at=53+(file[52]&255);need(at<file.length,"Неполный файл.");
            at+=1+96*(file[at]&255);need(at<file.length&&file.length-at==i32(file,12),"Неверный размер образа.");
            app=Arrays.copyOfRange(file,at,file.length);need(Arrays.equals(digest(app),Arrays.copyOfRange(file,16,48)),"Ошибка SHA-256 выпуска.");
        }
        need(embeddedImageEnd(app, 0, app.length, chip, true) == app.length && app[23] == 1,
            "Нужен отдельный образ приложения с SHA-256.");
        return app;
    }
    Map<String,int[]> layout(byte[] raw,String name)throws Exception{
        Map<String,int[]> expected=new HashMap<>();
        try(BufferedReader r=new BufferedReader(new InputStreamReader(assets.open("esp/"+name+".csv"),"UTF-8"))){
            String row;while((row=r.readLine())!=null){row=row.trim();if(row.isEmpty()||row.startsWith("#"))continue;
                String[] c=row.split(",");String label=c[0].trim();int type=c[1].trim().equals("app")?0:1;
                String sub=c[2].trim();int subtype=sub.equals("ota_0")?16:sub.equals("ota_1")?17:sub.equals("nvs")?2:sub.equals("ota")?0:6;
                expected.put(label,new int[]{Integer.decode(c[3].trim()),Integer.decode(c[4].trim()),type,subtype});}}
        Map<String, int[]> parts = partitions(raw);
        need(sameLayout(parts, expected), "Разметка платы отличается от схемы выпуска.");
        return parts;
    }

    static Map<String, int[]> partitions(byte[] raw) throws Exception {
        Map<String, int[]> parts = new HashMap<>();
        for (int offset = 0; offset + 32 <= raw.length; offset += 32) {
            int magic = u16(raw, offset);
            if (magic == 0xffff) break;
            if (magic == 0xebeb) {
                need(Arrays.equals(md5(Arrays.copyOf(raw, offset)), Arrays.copyOfRange(raw, offset + 16, offset + 32)),
                    "Ошибка контрольной суммы таблицы разделов.");
                break;
            }
            String label = field(raw, offset + 12, offset + 28);
            need(magic == 0x50aa && i32(raw, offset + 28) == 0 && !label.isEmpty() && !parts.containsKey(label),
                "Неподдерживаемая таблица разделов ESP.");
            parts.put(label, new int[]{i32(raw, offset + 4), i32(raw, offset + 8),
                raw[offset + 2] & 255, raw[offset + 3] & 255});
        }
        return parts;
    }

    private static boolean sameLayout(Map<String, int[]> parts, Map<String, int[]> expected) {
        if (parts.size() != expected.size()) return false;
        for (Map.Entry<String, int[]> entry : expected.entrySet()) {
            if (!Arrays.equals(entry.getValue(), parts.get(entry.getKey()))) return false;
        }
        return true;
    }

    private static final Map<String, int[]> STOCK_V3_LAYOUT = stockV3Layout();

    private static Map<String, int[]> stockV3Layout() {
        Map<String, int[]> parts = new HashMap<>();
        parts.put("nvs", new int[]{0x9000, 0x5000, 1, 2});
        parts.put("otadata", new int[]{0xe000, 0x2000, 1, 0});
        parts.put("app0", new int[]{APP_ADDRESS, 0x330000, 0, 16});
        parts.put("app1", new int[]{0x340000, 0x330000, 0, 17});
        parts.put("spiffs", new int[]{0x670000, 0x180000, 1, 130});
        parts.put("coredump", new int[]{0x7f0000, 0x10000, 1, 3});
        return parts;
    }

    static int seqCrc(int x){int crc=0;for(int i=0;i<4;i++){crc^=(x>>>(8*i))&255;for(int j=0;j<8;j++)crc=(crc>>>1)^((crc&1)!=0?0xedb88320:0);}return ~crc;}

    private static BootSlot bootSlot(byte[] records) throws IOException {
        long best = -1;
        int sector = 1;
        for (int index = 0; index < 2; index++) {
            int offset = index * SECTOR_SIZE;
            int rawSequence = i32(records, offset), state = i32(records, offset + 24);
            if (rawSequence == -1) continue;
            long sequence = rawSequence & 0xffffffffL;
            // PlatformIO boot_app0.bin has a zero-sequence record with an erased state.
            boolean initial = sequence == 0;
            for (int i = offset + 4; initial && i < offset + 32; i++) initial = records[i] == (byte) 0xff;
            need((initial || (sequence > 0 && sequence < 0xfffffffdL)) && i32(records, offset + 28) == seqCrc(rawSequence),
                "Повреждены данные выбора слота.");
            if (state >= -1 && state <= 2 && sequence > best) {
                best = sequence;
                sector = index;
            }
        }
        return new BootSlot(best < 0 ? 1 : best, sector);
    }

    private void selectSlot(int meta, BootSlot slot) throws Exception {
        int sequence = (int) (slot.sequence + 1);
        byte[] flag = new byte[32];
        Arrays.fill(flag, (byte) 0xff);
        ByteBuffer record = ByteBuffer.wrap(flag).order(ByteOrder.LITTLE_ENDIAN);
        record.putInt(0, sequence).putInt(24, 2).putInt(28, seqCrc(sequence));
        write(meta + (1 - slot.sector) * SECTOR_SIZE, flag);
    }

    private void enterBoot(int chip, String board, Progress progress, int size) throws Exception {
        progress.update("Вход в загрузчик ESP", 0, size);
        log.show("Вход в загрузчик " + board + "…");
        startBoot(chip, chip == 9 ? "esp32s3" : "esp32c6");
    }

    private void restart() throws Exception {
        wire.dtr(false);
        wire.rts(true);
        try { Thread.sleep(200); } finally { wire.rts(false); }
    }

    void installFactory(byte[] file, Progress progress) throws Exception {
        String target = factoryDescription(file);
        Board board = Board.fromId(file[5] & 255);
        byte[] app = validate(file, board.id, board.chip);
        byte[] prefix = load(assets.open("esp-factory/" + board.slug + ".bin"));
        need(prefix.length == PARTITION_TABLE + SECTOR_SIZE, "Повреждён встроенный образ загрузчика.");
        int[] appSlot = layout(Arrays.copyOfRange(prefix, PARTITION_TABLE, prefix.length), board.slug).get("ota_0");
        need(appSlot != null && appSlot[0] == APP_ADDRESS && app.length <= appSlot[1],
            "Приложение не помещается в первый загрузочный раздел.");
        byte[] image = Arrays.copyOf(prefix, APP_ADDRESS + app.length);
        Arrays.fill(image, prefix.length, APP_ADDRESS, (byte) 0xff);
        System.arraycopy(app, 0, image, APP_ADDRESS, app.length);
        log.show("Первая установка MeshCore-RS · " + target);
        installMerged(image, true, progress);
    }

    void installMerged(byte[] file, boolean erase, Progress progress) throws Exception {
        need(file.length > APP_ADDRESS + 36 && (file[0] & 255) == 0xe9,
            "В полном .bin должны быть загрузчик, таблица разделов и приложение ESP.");
        int chip = u16(file, 12);
        need(chip == 9 || chip == 13, "Полный .bin должен быть для ESP32-S3 или ESP32-C6.");
        int sizeCode = (file[3] & 255) >>> 4;
        need(sizeCode <= 7, "Неизвестный размер флеш-памяти в образе ESP.");
        int requiredCapacity = (1 << sizeCode) * 1024 * 1024;
        need(file.length <= requiredCapacity, "Полный образ превышает размер флеш-памяти.");
        embeddedImageEnd(file, 0, PARTITION_TABLE, chip, false);
        Map<String, int[]> parts = partitions(Arrays.copyOfRange(file, PARTITION_TABLE, PARTITION_TABLE + SECTOR_SIZE));
        List<int[]> ordered = new ArrayList<>(parts.values());
        Collections.sort(ordered, (left, right) -> Integer.compare(left[0], right[0]));
        long endOfPartition = PARTITION_TABLE + SECTOR_SIZE;
        int appSize = 0;
        for (int[] part : ordered) {
            need(part[0] >= endOfPartition && part[1] > 0 && part[0] % SECTOR_SIZE == 0 && part[1] % SECTOR_SIZE == 0
                && (long) part[0] + part[1] <= requiredCapacity, "Разделы полного образа пересекаются или выходят за пределы памяти.");
            endOfPartition = (long) part[0] + part[1];
            if (part[0] == APP_ADDRESS && part[2] == 0 && (part[3] == 0 || part[3] == 16)) appSize = part[1];
        }
        need(appSize > 0, "В полном образе отсутствует первый загрузочный раздел.");
        embeddedImageEnd(file, APP_ADDRESS, Math.min(file.length, APP_ADDRESS + appSize), chip, true);
        try {
            enterBoot(chip, chip == 9 ? "ESP32-S3" : "ESP32-C6", progress, file.length);
            int capacity = flashCapacity(chip);
            need(capacity >= requiredCapacity, "Размер флеш-памяти платы не соответствует полному образу.");
            configureFlash(capacity);
            if (erase) {
                progress.update("Полная очистка флеш-памяти", 0, file.length);
                command(0xd0, new byte[0], 0, 120000);
            }
            write(0, file, progress);
        } finally { restart(); }
        log.show("Запись полного образа проверена. Плата перезапускается.");
    }

    void installStockV3(byte[] file, Progress progress) throws Exception {
        int chip = Board.HELTEC_V3.chip;
        need(embeddedImageEnd(file, 0, file.length, chip, true) == file.length,
            "Нужен отдельный образ приложения Heltec V3.");
        try {
            enterBoot(chip, "Heltec V3", progress, file.length);
            int capacity = flashCapacity(chip);
            need(capacity == 8 * 1024 * 1024, "Для этого образа Heltec V3 нужны 8 МиБ флеш-памяти.");
            configureFlash(capacity);
            Map<String, int[]> parts = partitions(read(PARTITION_TABLE, SECTOR_SIZE));
            need(sameLayout(parts, STOCK_V3_LAYOUT), "Разметка платы отличается от штатной MeshCore для Heltec V3. Выберите полный *-merged.bin.");
            int meta = parts.get("otadata")[0];
            BootSlot slot = bootSlot(read(meta, 2 * SECTOR_SIZE));
            int[] app = parts.get("app" + slot.inactive());
            need(file.length <= app[1], "Образ не помещается в раздел приложения.");
            write(app[0], file, progress);
            selectSlot(meta, slot);
        } finally { restart(); }
        log.show("Запись приложения проверена. Плата перезапускается.");
    }

    void install(byte[] file,Ota.Device d,boolean clean)throws Exception{
        install(file,d,clean,Progress.NONE);
    }
    void install(byte[] file,Ota.Device d,boolean clean,Progress progress)throws Exception{
        Board board = Board.fromId(d.board);
        byte[] app = validate(file, board.id, board.chip);
        try{
            enterBoot(board.chip, board.slug, progress, app.length);
            progress.update("Проверка разделов ESP",0,app.length);
            Map<String,int[]> pt=layout(read(PARTITION_TABLE,SECTOR_SIZE),board.slug);
            int meta=pt.get("otadata")[0];
            BootSlot slot=bootSlot(read(meta,2*SECTOR_SIZE));
            int[] old=pt.get("ota_"+(1-slot.inactive())),next=pt.get("ota_"+slot.inactive());
            need(d.runningSize>0&&d.runningSize<=old[1]&&app.length<=next[1],"Неверный размер раздела или прошивки.");
            need(Arrays.equals(Arrays.copyOf(digest(read(old[0],d.runningSize)),8),d.hash),"Активный слот не подтверждён.");
            write(next[0],app,progress);
            if(clean)progress.update("Очистка данных ноды",app.length,app.length);
            if(clean)for(String key:new String[]{"nvs","meshcore","companion"}){int[] x=pt.get(key);log.show("Очистка "+key+"…");command(0xd1,ints(x[0],x[1]),0,60000);byte[] blank=new byte[x[1]];Arrays.fill(blank,(byte)0xff);verify(x[0],blank);}
            progress.update("Выбор нового слота ESP",app.length,app.length);
            selectSlot(meta,slot);
        }finally{restart();}
        log.show("Запись проверена. После перезагрузки нажмите «Проверить устройство».");
    }
}

