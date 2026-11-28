package ru.ommesh.otg;
import android.bluetooth.*;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.Build;
import java.io.*;
import java.util.*;
import java.util.concurrent.*;

final class BlePort extends BluetoothGattCallback implements Ota.Port, Closeable, LegacyDfu.Link {
    static final UUID DFU=UUID.fromString("00001530-1212-efde-1523-785feabcd123"),
        CONTROL=UUID.fromString("00001531-1212-efde-1523-785feabcd123"),
        PACKET=UUID.fromString("00001532-1212-efde-1523-785feabcd123");
    static final UUID SERVICE=UUID.fromString("6e400001-b5a3-f393-e0a9-e50e24dcca9e"),
        RX=UUID.fromString("6e400002-b5a3-f393-e0a9-e50e24dcca9e"),TX=UUID.fromString("6e400003-b5a3-f393-e0a9-e50e24dcca9e"),
        CCC=UUID.fromString("00002902-0000-1000-8000-00805f9b34fb");
    private static final String SUBSCRIPTION = "подписка BLE";
    private static final int ATT_INVALID_HANDLE = 0x01;
    private static final int ATT_ATTRIBUTE_NOT_FOUND = 0x0a;
    private static final int ATT_DATABASE_OUT_OF_SYNC = 0x12;
    static final class Event {
        final String stage;
        final int status;
        Event(String stage,int status){this.stage=stage;this.status=status;}
    }
    static final class GattFailure extends IOException {
        final String stage;
        final int status;
        GattFailure(String stage,int status){
            super("Bluetooth, "+stage+" (GATT "+status+").");
            this.stage=stage;
            this.status=status;
        }
        boolean needsServiceDiscovery() {
            return SUBSCRIPTION.equals(stage) && (status == ATT_INVALID_HANDLE
                || status == ATT_ATTRIBUTE_NOT_FOUND || status == ATT_DATABASE_OUT_OF_SYNC);
        }
    }
    final BlockingQueue<Event> events=new LinkedBlockingQueue<>();
    final BlockingQueue<byte[]> frames=new LinkedBlockingQueue<>();
    BluetoothGatt gatt;BluetoothGattCharacteristic rx,control;
    volatile boolean disconnected;volatile int mtu=23;

    private BlePort() {}

    static BlePort open(Context context, BluetoothDevice device, boolean dfu, Ota.Log log) throws IOException {
        for (int attempt = 0; ; attempt++) {
            BlePort port = new BlePort();
            boolean ready = false;
            try {
                port.connect(context, device, dfu);
                ready = true;
                return port;
            } catch (GattFailure failure) {
                if (dfu || !failure.needsServiceDiscovery()) throw failure;
                if (attempt > 0) {
                    throw new IOException("Плата не приняла подписку на ответы BLE (GATT " + failure.status
                        + "). Удалите сопряжение с этой платой в настройках Android, снова откройте Bluetooth"
                        + " на плате и нажмите «Проверить устройство».", failure);
                }
                log.show("Bluetooth (GATT " + failure.status + "). Переподключение и повторный поиск служб…");
                port.refreshServiceCache();
            } finally {
                if (!ready) port.close();
            }
            // Give Android time to close the old link and the board time to advertise again.
            try {
                Thread.sleep(750);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                throw new IOException("Повторное подключение Bluetooth прервано.", e);
            }
        }
    }

    private void connect(Context context, BluetoothDevice device, boolean dfu) throws IOException {
        gatt = device.connectGatt(context, false, this, BluetoothDevice.TRANSPORT_LE);
        if (gatt == null) throw new IOException("Не удалось открыть Bluetooth.");
        await("connect", 25000);
        if (!gatt.discoverServices()) throw new IOException("Не удалось запросить службы BLE.");
        await("services", 15000);
        BluetoothGattService service = gatt.getService(dfu ? DFU : SERVICE);
        if (service == null) {
            throw new IOException(dfu ? "Нет службы Nordic Legacy DFU. Переведите плату в режим BLE OTA."
                : "Нет службы MeshCore UART. Откройте окно BLE-обновления на устройстве.");
        }
        rx = service.getCharacteristic(dfu ? PACKET : RX);
        BluetoothGattCharacteristic tx = service.getCharacteristic(dfu ? CONTROL : TX);
        if (dfu) control = tx;
        if (rx == null || tx == null) throw new IOException("Недоступен канал BLE.");
        if (!dfu && gatt.requestMtu(247)) await("mtu", 12000);
        enableUpdates(context, device, tx, dfu);
        gatt.requestConnectionPriority(BluetoothGatt.CONNECTION_PRIORITY_HIGH);
    }

    private void refreshServiceCache() {
        // Some Android versions expose this method. Reconnect even when it is unavailable.
        try {
            BluetoothGatt.class.getMethod("refresh").invoke(gatt);
        } catch (ReflectiveOperationException | SecurityException unavailable) {
            // Service discovery still runs on the new connection.
        }
    }

    void enableUpdates(Context context,BluetoothDevice device,BluetoothGattCharacteristic tx,boolean dfu)throws IOException{
        BluetoothGattDescriptor descriptor=tx.getDescriptor(CCC);
        boolean notify=(tx.getProperties()&BluetoothGattCharacteristic.PROPERTY_NOTIFY)!=0;
        boolean indicate=(tx.getProperties()&BluetoothGattCharacteristic.PROPERTY_INDICATE)!=0;
        if(descriptor==null||(!notify&&!indicate)||!gatt.setCharacteristicNotification(tx,true))
            throw new IOException("Недоступны ответы устройства по BLE.");
        byte[] config=notify?BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE:
            BluetoothGattDescriptor.ENABLE_INDICATION_VALUE;
        try{
            subscribe(descriptor,config);
        }catch(GattFailure first){
            boolean unsupported=first.status==BluetoothGatt.GATT_REQUEST_NOT_SUPPORTED;
            boolean protectedAttribute=first.status==BluetoothGatt.GATT_INSUFFICIENT_AUTHENTICATION||
                first.status==BluetoothGatt.GATT_INSUFFICIENT_ENCRYPTION;
            boolean useIndications=unsupported&&notify&&indicate;
            if(dfu||(!unsupported&&!protectedAttribute))throw first;
            if(device.getBondState()!=BluetoothDevice.BOND_BONDED)waitForBond(context,device);
            try{
                subscribe(descriptor,useIndications?BluetoothGattDescriptor.ENABLE_INDICATION_VALUE:config);
            }catch(GattFailure second){
                if(second.needsServiceDiscovery())throw second;
                throw new IOException("Плата отклонила подписку на ответы BLE (GATT "+second.status+
                    "). Закройте другие подключения к плате, снова откройте Bluetooth на плате и повторите поиск.",second);
            }
        }
    }
    void subscribe(BluetoothGattDescriptor descriptor,byte[] config)throws IOException{
        boolean sent;
        if(Build.VERSION.SDK_INT>=33){
            sent=gatt.writeDescriptor(descriptor,config)==BluetoothStatusCodes.SUCCESS;
        }else{
            sent=descriptor.setValue(config)&&gatt.writeDescriptor(descriptor);
        }
        if(!sent)throw new IOException("Не удалось запросить ответы устройства по BLE.");
        await(SUBSCRIPTION,30000);
    }
    static void waitForBond(Context context,BluetoothDevice device)throws IOException{
        CountDownLatch result=new CountDownLatch(1);
        BroadcastReceiver receiver=new BroadcastReceiver(){
            @Override public void onReceive(Context c,Intent intent){
                BluetoothDevice other=intent.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE);
                int state=intent.getIntExtra(BluetoothDevice.EXTRA_BOND_STATE,BluetoothDevice.BOND_NONE);
                if(device.equals(other)&&state!=BluetoothDevice.BOND_BONDING)result.countDown();
            }
        };
        IntentFilter filter=new IntentFilter(BluetoothDevice.ACTION_BOND_STATE_CHANGED);
        if(Build.VERSION.SDK_INT>=33)context.registerReceiver(receiver,filter,Context.RECEIVER_EXPORTED);
        else context.registerReceiver(receiver,filter);
        try{
            if(device.getBondState()==BluetoothDevice.BOND_NONE&&!device.createBond())
                throw new IOException("Телефон не смог начать сопряжение. Откройте Bluetooth на плате и повторите попытку.");
            if(device.getBondState()!=BluetoothDevice.BOND_BONDED)result.await(45,TimeUnit.SECONDS);
            if(device.getBondState()!=BluetoothDevice.BOND_BONDED)
                throw new IOException("Сопряжение не завершено. Подтвердите запрос Android и введите PIN с платы, если он запрошен.");
        }catch(InterruptedException e){
            Thread.currentThread().interrupt();throw new IOException(e);
        }finally{context.unregisterReceiver(receiver);}
    }
    void await(String what,int ms)throws IOException{
        long end=System.nanoTime()+ms*1000000L;
        try{
            while(!disconnected){Event result=events.poll(Math.max(1,(end-System.nanoTime())/1000000),TimeUnit.MILLISECONDS);
                if(result==null||System.nanoTime()>end)throw new IOException("Нет ответа BLE ("+what+").");
                if(disconnected)break;
                if(result.stage.equals(what)){
                    if(result.status!=BluetoothGatt.GATT_SUCCESS)throw new GattFailure(what,result.status);
                    return;
                }
            }
        }catch(InterruptedException e){Thread.currentThread().interrupt();throw new IOException(e);}
        throw new IOException("Связь Bluetooth прервана.");
    }
    void event(String name,int status){events.offer(new Event(name,status));}
    public void onConnectionStateChange(BluetoothGatt g,int status,int state){
        if(state==BluetoothProfile.STATE_DISCONNECTED){disconnected=true;event("disconnect",status);}
        else if(state==BluetoothProfile.STATE_CONNECTED)event("connect",status);
    }
    public void onServicesDiscovered(BluetoothGatt g,int s){event("services",s);}
    public void onMtuChanged(BluetoothGatt g,int m,int s){if(s==0)mtu=m;event("mtu",BluetoothGatt.GATT_SUCCESS);}
    public void onDescriptorWrite(BluetoothGatt g,BluetoothGattDescriptor d,int s){
        if(CCC.equals(d.getUuid()))event(SUBSCRIPTION,s);
    }
    public void onCharacteristicWrite(BluetoothGatt g,BluetoothGattCharacteristic c,int s){event("write",s);}
    public void onCharacteristicChanged(BluetoothGatt g,BluetoothGattCharacteristic c){if(Build.VERSION.SDK_INT<33)received(c,c.getValue());}
    public void onCharacteristicChanged(BluetoothGatt g,BluetoothGattCharacteristic c,byte[] value){received(c,value);}
    void received(BluetoothGattCharacteristic c,byte[] b){
        if(CONTROL.equals(c.getUuid())&&b!=null){frames.offer(b.clone());return;}
        if(TX.equals(c.getUuid())&&b!=null&&b.length>0&&b.length<=300)
            frames.offer(BleFrames.incoming(b));
    }
    public void control(byte... value)throws IOException{
        control.setWriteType(BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT);control.setValue(value);
        if(!gatt.writeCharacteristic(control))throw new IOException("Не удалось отправить команду DFU.");
        try{await("write",15000);}catch(IOException e){if((value[0]!=5 && value[0]!=6)||!disconnected)throw e;}
    }
    public void packet(byte[] value)throws IOException{
        rx.setWriteType(BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE);rx.setValue(value);
        if(!gatt.writeCharacteristic(rx))throw new IOException("Не удалось отправить пакет DFU.");
        await("write",15000);
    }
    public byte[] notice()throws IOException{
        try{
            for(int i=0;i<120;i++){
                byte[] r=frames.poll(500,TimeUnit.MILLISECONDS);if(r!=null)return r;
                if(disconnected)throw new IOException("Загрузчик отключился.");
            }
            throw new IOException("Истекло время ожидания ответа DFU.");
        }catch(InterruptedException e){Thread.currentThread().interrupt();throw new IOException(e);}
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

