package ru.ommesh.otg;

import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.*;
import java.util.zip.*;

final class LegacyPackage {
    static final int START=0x26000, END=0xea000;
    final byte[] image, init;
    final int crc;
    private LegacyPackage(byte[] image){
        this.image=image;
        int value=0xffff;
        for(byte b:image){
            value ^= (b&255)<<8;
            for(int i=0;i<8;i++) value=((value<<1)^((value&0x8000)!=0?0x1021:0))&0xffff;
        }
        crc=value;
        init=Ota.le(14).putShort((short)0x52).putShort((short)-1).putInt(-1)
            .putShort((short)1).putShort((short)0xb6).putShort((short)crc).array();
    }
    static LegacyPackage fromUf2(byte[] file)throws Exception{
        int count=file.length/512;
        Ota.need(count>0 && file.length%512==0 && count<=END-START,"Повреждён размер UF2.");
        byte[] flash=new byte[END-START];
        BitSet blocks=new BitSet(count),used=new BitSet(flash.length);
        int end=0;
        for(int p=0;p<file.length;p+=512){
            Ota.need(Ota.i32(file,p)==0x0a324655 && Ota.i32(file,p+4)==0x9e5d5157
                && Ota.i32(file,p+508)==0x0ab16f30,"Повреждён блок UF2.");
            Ota.need(Ota.i32(file,p+8)==0x2000 && Ota.i32(file,p+28)==0xada52840,
                "Нужен UF2 приложения nRF52840.");
            int addr=Ota.i32(file,p+12),len=Ota.i32(file,p+16),num=Ota.i32(file,p+20);
            Ota.need(len>0 && len<=476 && addr>=START && addr<=END-len,
                "UF2 содержит данные вне области приложения S140 6.1.1.");
            Ota.need(Ota.i32(file,p+24)==count && num>=0 && num<count && !blocks.get(num),
                "Неполный UF2 или повторяющийся номер блока.");
            blocks.set(num);
            int off=addr-START, next=used.nextSetBit(off);
            Ota.need(next<0 || next>=off+len,"Адреса блоков UF2 перекрываются.");
            used.set(off,off+len);
            System.arraycopy(file,p+32,flash,off,len);
            end=Math.max(end,off+len);
        }
        Ota.need(end>=8 && used.nextClearBit(0)==end && (end&3)==0,
            "Нужен непрерывный образ приложения с адреса 0x26000.");
        int stack=Ota.i32(flash,0),reset=Ota.i32(flash,4);
        Ota.need(stack>0x20000000 && stack<=0x20040000 && (stack&3)==0 &&
            (reset&1)==1 && reset>=START && reset<START+end,"Неверная таблица запуска приложения.");
        byte[] magic="MCRS-NRF".getBytes(StandardCharsets.US_ASCII);
        // MeshCore-RS pads its final UF2 block. Its trailer records the exact image length.
        for(int i=end-64;i>=Math.max(0,end-64-475);i--){
            boolean match=true;
            for(int k=0;k<magic.length;k++) if(flash[i+k]!=magic[k]){match=false;break;}
            if(!match || Ota.i32(flash,i+8)!=i) continue;
            Ota.need(Ota.i32(flash,i+12)==START && Ota.i32(flash,i+16)==0x52840 &&
                Arrays.equals(Ota.digest(Arrays.copyOf(flash,i)),Arrays.copyOfRange(flash,i+32,i+64)),
                "Контрольная сумма или адрес образа MeshCore-RS не совпадает.");
            for(int k=i+64;k<end;k++) Ota.need(flash[k]==0 || flash[k]==-1,"Лишние данные после образа.");
            end=i+64;
            break;
        }
        return new LegacyPackage(Arrays.copyOf(flash,end));
    }
    void save(OutputStream out)throws IOException{
        String json="{\"manifest\":{\"application\":{\"bin_file\":\"firmware.bin\",\"dat_file\":\"firmware.dat\","
            +"\"init_packet_data\":{\"application_version\":4294967295,\"device_revision\":65535,"
            +"\"device_type\":82,\"firmware_crc16\":"+crc+",\"softdevice_req\":[182]}},\"dfu_version\":0.5}}";
        try(ZipOutputStream zip=new ZipOutputStream(out)){
            entry(zip,"firmware.bin",image);entry(zip,"firmware.dat",init);
            entry(zip,"manifest.json",json.getBytes(StandardCharsets.UTF_8));
        }
    }
    private static void entry(ZipOutputStream zip,String name,byte[] bytes)throws IOException{
        zip.putNextEntry(new ZipEntry(name));zip.write(bytes);zip.closeEntry();
    }
}
