package ru.ommesh.otg;
import java.io.IOException;
import java.util.Arrays;
final class BleFrames{
    static byte[] outgoing(byte[] stream)throws IOException{
        Ota.need(stream.length>=4&&stream[0]=='<'&&Ota.u16(stream,1)==stream.length-3,"Неверный формат BLE-пакета.");
        return Arrays.copyOfRange(stream,3,stream.length);
    }
    static byte[] incoming(byte[] gatt){return Ota.le(3+gatt.length).put((byte)'>').putShort((short)gatt.length).put(gatt).array();}
}
