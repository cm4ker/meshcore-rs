package ru.ommesh.otg;
import android.bluetooth.*;
import android.content.Context;
import android.os.Build;
import java.io.*;
import java.util.*;
import java.util.concurrent.*;

final class BlePort extends BluetoothGattCallback implements Ota.Port, Closeable {
    static final UUID SERVICE=UUID.fromString("6e400001-b5a3-f393-e0a9-e50e24dcca9e"),
        RX=UUID.fromString("6e400002-b5a3-f393-e0a9-e50e24dcca9e"),TX=UUID.fromString("6e400003-b5a3-f393-e0a9-e50e24dcca9e"),
        CCC=UUID.fromString("00002902-0000-1000-8000-00805f9b34fb");
    final BlockingQueue<String> events=new LinkedBlockingQueue<>();
    final BlockingQueue<byte[]> frames=new LinkedBlockingQueue<>();
    BluetoothGatt gatt;BluetoothGattCharacteristic rx;
    volatile boolean disconnected;volatile int mtu=23;
    BlePort(Context c,BluetoothDevice device)throws IOException {
        try{
            gatt=device.connectGatt(c,false,this,BluetoothDevice.TRANSPORT_LE);
            if(gatt==null)throw new IOException("Не удалось открыть Bluetooth.");
            await("connect",25000);
            if(!gatt.discoverServices())throw new IOException("Не удалось запросить службы BLE.");
            await("services",15000);
            BluetoothGattService service=gatt.getService(SERVICE);
            if(service==null)throw new IOException("Нет службы MeshCore UART. Откройте окно BLE-обновления на устройстве.");
            rx=service.getCharacteristic(RX);BluetoothGattCharacteristic tx=service.getCharacteristic(TX);
            if(rx==null||tx==null||!gatt.setCharacteristicNotification(tx,true))throw new IOException("Недоступен канал MeshCore BLE.");
            BluetoothGattDescriptor desc=tx.getDescriptor(CCC);
            if(desc==null)throw new IOException("Недоступны уведомления BLE.");
            desc.setValue(BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE);
            if(!gatt.writeDescriptor(desc))throw new IOException("Не удалось включить уведомления.");
            await("notify",30000);
            if(gatt.requestMtu(247))await("mtu",12000);
            gatt.requestConnectionPriority(BluetoothGatt.CONNECTION_PRIORITY_HIGH);
        }catch(Exception e){close();throw new IOException(e.getMessage(),e);}
    }
    void await(String what,int ms)throws IOException{
        long end=System.nanoTime()+ms*1000000L;
        try{
            while(!disconnected){String result=events.poll(Math.max(1,(end-System.nanoTime())/1000000),TimeUnit.MILLISECONDS);
                if(result==null||System.nanoTime()>end)throw new IOException("Нет ответа BLE ("+what+").");
                if(result.equals(what))return;
                if(result.startsWith("!"))throw new IOException(result.substring(1));
            }
        }catch(InterruptedException e){Thread.currentThread().interrupt();throw new IOException(e);}
        throw new IOException("Связь Bluetooth прервана.");
    }
    void event(String name,int status){events.offer(status==0?name:"!Ошибка Bluetooth • "+status+" ("+name+")");}
    public void onConnectionStateChange(BluetoothGatt g,int status,int state){
        if(state==BluetoothProfile.STATE_DISCONNECTED){disconnected=true;events.offer("!Устройство отключилось.");}
        else if(state==BluetoothProfile.STATE_CONNECTED)event("connect",status);
    }
    public void onServicesDiscovered(BluetoothGatt g,int s){event("services",s);}
    public void onMtuChanged(BluetoothGatt g,int m,int s){if(s==0)mtu=m;events.offer("mtu");}
    public void onDescriptorWrite(BluetoothGatt g,BluetoothGattDescriptor d,int s){event("notify",s);}
    public void onCharacteristicWrite(BluetoothGatt g,BluetoothGattCharacteristic c,int s){event("write",s);}
    public void onCharacteristicChanged(BluetoothGatt g,BluetoothGattCharacteristic c){if(Build.VERSION.SDK_INT<33)received(c,c.getValue());}
    public void onCharacteristicChanged(BluetoothGatt g,BluetoothGattCharacteristic c,byte[] value){received(c,value);}
    void received(BluetoothGattCharacteristic c,byte[] b){
        if(TX.equals(c.getUuid())&&b!=null&&b.length>0&&b.length<=300)
            frames.offer(BleFrames.incoming(b));
    }
    public void write(byte[] framed)throws IOException{
        byte[] b=BleFrames.outgoing(framed);
        if(b.length>mtu-3)throw new IOException("Согласованный размер пакета BLE слишком мал. Переподключитесь или используйте USB.");
        boolean noResponse=(b[0]&255)==0xf3&&(rx.getProperties()&BluetoothGattCharacteristic.PROPERTY_WRITE_NO_RESPONSE)!=0;
        rx.setWriteType(noResponse?BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE:BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT);
        rx.setValue(b);if(!gatt.writeCharacteristic(rx))throw new IOException("Ошибка отправки по BLE.");
        if(!noResponse)await("write",(b[0]&255)==0xf3?120000:30000);
    }
    public int read(byte[] b,int ms)throws IOException{
        try{byte[] f=frames.poll(ms,TimeUnit.MILLISECONDS);if(f!=null){System.arraycopy(f,0,b,0,f.length);return f.length;}}
        catch(InterruptedException e){Thread.currentThread().interrupt();throw new IOException(e);}
        if(disconnected)throw new IOException("Устройство BLE отключилось.");return 0;
    }
    public void close(){if(gatt!=null){try{gatt.disconnect();}finally{gatt.close();gatt=null;}}}
}

