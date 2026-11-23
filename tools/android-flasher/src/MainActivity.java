package ru.ommesh.otg;

import android.app.*;
import android.bluetooth.*;
import android.bluetooth.le.*;
import android.content.pm.PackageManager;
import android.Manifest;
import android.os.*;
import android.content.*;
import android.database.Cursor;
import android.hardware.usb.*;
import android.net.Uri;
import android.provider.OpenableColumns;
import android.view.*;
import android.widget.*;
import com.hoho.android.usbserial.driver.*;
import java.io.*;
import java.util.*;

public class MainActivity extends Activity {
    static final String PERMISSION="ru.ommesh.otg.USB_PERMISSION";
    UsbManager usb;
    Spinner devices;
    TextView fileName, report;
    ScrollView scroll;
    Button scan, selectFile, info, update;
    final List<UsbSerialDriver> drivers=new ArrayList<>();
    Uri file;
    UsbDevice pending;
    String requested="info";
    boolean busy, scanning;
    Spinner method;
    EditText host;
    Button flashUsb, resetNode;
    TextView guide;
    BluetoothLeScanner scanner;
    ScanCallback callback;
    final Handler handler=new Handler(Looper.getMainLooper());
    final ArrayList<BluetoothDevice> found=new ArrayList<>();
    final ArrayList<String> bleLabels=new ArrayList<>();
    int mode(){return method.getSelectedItemPosition();}
    final BroadcastReceiver permissionReceiver=new BroadcastReceiver(){
        public void onReceive(Context c,Intent event){
            if(!PERMISSION.equals(event.getAction())||pending==null)return;
            UsbDevice target=pending; pending=null;
            if(usb.hasPermission(target))runOperation(target,requested);
            else{line("Доступ к USB не предоставлен.");idle();}
        }
    };
    final BroadcastReceiver devicesReceiver=new BroadcastReceiver(){
        public void onReceive(Context c,Intent event){
            if(!busy&&mode()==0)refresh();
            else if(pending!=null&&!usb.getDeviceList().containsKey(pending.getDeviceName())){
                pending=null;line("Устройство отключено.");idle();
            }
        }
    };
    @Override public void onCreate(Bundle saved){
        super.onCreate(saved);
        usb=(UsbManager)getSystemService(USB_SERVICE);
        ScrollView page=new ScrollView(this);page.setFillViewport(true);setContentView(page);LinearLayout base=new LinearLayout(this);page.addView(base);base.setOrientation(1);base.setPadding(dp(16),dp(12),dp(16),dp(10));
        method=new Spinner(this,Spinner.MODE_DROPDOWN);method.setPopupBackgroundDrawable(new android.graphics.drawable.ColorDrawable(0xfffafafa));base.addView(method);method.setAdapter(new ArrayAdapter<String>(this,getResources().getIdentifier("port_item","layout",getPackageName()),new String[]{"USB OTG","Wi-Fi / TCP","Bluetooth LE"}));
        guide=new TextView(this);guide.setText("Подключите MeshCore кабелем с передачей данных через USB OTG.");base.addView(guide);
        devices=new Spinner(this,Spinner.MODE_DROPDOWN);devices.setPopupBackgroundDrawable(new android.graphics.drawable.ColorDrawable(0xfffafafa));base.addView(devices);
        host=new EditText(this);host.setSingleLine(true);host.setHint("192.168.1.50:5000");host.setText(getPreferences(0).getString("host",""));base.addView(host);
        scan=button(base,"Найти устройства",v->{if(mode()==2)scanBle();else refresh();});
        button(base,"ИНСТРУКЦИЯ",v->new AlertDialog.Builder(this).setTitle("ИНСТРУКЦИЯ")
            .setMessage("Для проверки связи нажмите «Версия устройства». Обновление по Wi-Fi использует TCP-сервис платы, обычно порт 5000. Для BLE включите Bluetooth и откройте окно обновления на плате, если оно требуется. При запросе Android введите PIN устройства.\n\nДля .mcfd выберите «Обновить прошивку». Дельта подходит только к своей исходной сборке.\n\n«Прошить через USB» устанавливает образ через загрузчик на поддерживаемой ESP с работающей MeshCore-RS. «Новая нода со сбросом» также удаляет ключ и пользовательские данные обеих ролей.")
            .setPositiveButton("Закрыть",null).show());
        fileName=new TextView(this);fileName.setPadding(0,dp(10),0,0);base.addView(fileName);
        selectFile=button(base,"Выбрать файл прошивки",v->{Intent i=new Intent(Intent.ACTION_OPEN_DOCUMENT);i.setType("*/*");i.addCategory(Intent.CATEGORY_OPENABLE);startActivityForResult(i,1);});
        info=button(base,"Версия устройства",v->ask("info"));
        update=button(base,"Обновить прошивку",v->ask("update"));
        flashUsb=button(base,"Прошить через USB",v->ask("flash"));
        resetNode=button(base,"Новая нода со сбросом…",v->ask("reset"));
        report=new TextView(this);report.setTextIsSelectable(true);
        scroll=new ScrollView(this);scroll.addView(report);base.addView(scroll,new LinearLayout.LayoutParams(-1,dp(220)));
        if(saved!=null && saved.getString("file")!=null)file=Uri.parse(saved.getString("file"));
        showFile();
        IntentFilter hotplug=new IntentFilter(UsbManager.ACTION_USB_DEVICE_ATTACHED);hotplug.addAction(UsbManager.ACTION_USB_DEVICE_DETACHED);
        if(Build.VERSION.SDK_INT>=33){
            registerReceiver(permissionReceiver,new IntentFilter(PERMISSION),Context.RECEIVER_NOT_EXPORTED);
            registerReceiver(devicesReceiver,hotplug,Context.RECEIVER_EXPORTED);
        }else{registerReceiver(permissionReceiver,new IntentFilter(PERMISSION));registerReceiver(devicesReceiver,hotplug);}
        method.setOnItemSelectedListener(new AdapterView.OnItemSelectedListener(){
            public void onNothingSelected(AdapterView<?> v){}
            public void onItemSelected(AdapterView<?> v,View item,int p,long id){stopScan();refresh();}
        });
        if(saved!=null)method.setSelection(saved.getInt("mode",0));
        refresh();
    }
    int dp(int value){return Math.round(getResources().getDisplayMetrics().density*value);}
    Button button(LinearLayout base,String text,android.view.View.OnClickListener clicked){
        Button b=new Button(this);b.setAllCaps(false);b.setText(text);base.addView(b);b.setOnClickListener(clicked);return b;
    }
    void refresh(){
        int m=mode();
        guide.setText(m==0?"Подключите плату кабелем с передачей данных через USB OTG.":m==1?"Введите адрес платы в вашей Wi-Fi-сети. Порт по умолчанию 5000.":"Откройте окно подключения на плате и запустите поиск MeshCore.");
        host.setVisibility(m==1?View.VISIBLE:View.GONE);devices.setVisibility(m==1?View.GONE:View.VISIBLE);scan.setVisibility(m==1?View.GONE:View.VISIBLE);
        flashUsb.setVisibility(m==0?View.VISIBLE:View.GONE);resetNode.setVisibility(m==0?View.VISIBLE:View.GONE);
        scan.setText(m==2?"Найти Bluetooth-устройства":"Найти USB-устройства");
        if(m==1){controls();return;}
        if(m==2){showBle();return;}
        int idx=devices.getSelectedItemPosition();String old=idx>=0&&idx<drivers.size()?drivers.get(idx).getDevice().getDeviceName():"";
        drivers.clear();drivers.addAll(UsbSerialProber.getDefaultProber().findAllDrivers(usb));
        List<String> labels=new ArrayList<>();int current=0;
        for(int k=0;k<drivers.size();k++){UsbDevice dev=drivers.get(k).getDevice();String name=dev.getProductName();
            labels.add(String.format(Locale.ROOT,"%s (%04X:%04X) · %s",name==null?"USB Serial":name,dev.getVendorId(),dev.getProductId(),dev.getDeviceName()));
            if(old.equals(dev.getDeviceName()))current=k;
        }
        if(labels.isEmpty())labels.add("USB-устройство не найдено");
        ArrayAdapter<String>a=new ArrayAdapter<>(this,getResources().getIdentifier("port_item","layout",getPackageName()),labels);devices.setAdapter(a);devices.setSelection(current);
        controls();
    }
    void controls(){
        boolean connected=mode()==1||(mode()==2?!found.isEmpty():!drivers.isEmpty());method.setEnabled(!busy);host.setEnabled(!busy);flashUsb.setEnabled(!busy&&connected&&file!=null);resetNode.setEnabled(!busy&&connected&&file!=null);
        devices.setEnabled(!busy&&connected);scan.setEnabled(!busy);selectFile.setEnabled(!busy);
        info.setEnabled(!busy&&connected);update.setEnabled(!busy&&connected&&file!=null);
    }
    String displayName="";
    void showFile(){
        displayName="";
        if(file!=null){
            try(Cursor c=getContentResolver().query(file,new String[]{OpenableColumns.DISPLAY_NAME},null,null,null)){
                if(c!=null&&c.moveToFirst())displayName=c.getString(0);
            }catch(RuntimeException ignored){}
            if(displayName==null||displayName.isEmpty())displayName="Выбранный файл";
        }
        fileName.setText(file==null?"Выберите файл .mcfw, .mcfd или .bin":displayName);
    }
    @Override protected void onActivityResult(int request,int result,Intent data){
        super.onActivityResult(request,result,data);
        if(request==1&&result==RESULT_OK&&data!=null){file=data.getData();showFile();controls();}
    }
    @Override protected void onSaveInstanceState(Bundle b){super.onSaveInstanceState(b);b.putInt("mode",mode());if(file!=null)b.putString("file",file.toString());}
    void line(String msg){runOnUiThread(()->{if(!isDestroyed()){report.append(msg+"\n");scroll.post(()->scroll.fullScroll(View.FOCUS_DOWN));}});}
        void showBle(){
        int previous=Math.max(devices.getSelectedItemPosition(),0);
        List<String> labels=new ArrayList<>(bleLabels);if(labels.isEmpty())labels.add("Устройство не выбрано");
        devices.setAdapter(new ArrayAdapter<>(this,getResources().getIdentifier("port_item","layout",getPackageName()),labels));
        devices.setSelection(Math.min(previous,labels.size()-1));controls();
    }
    String[] blePermissions(){return Build.VERSION.SDK_INT>=31?
        new String[]{Manifest.permission.BLUETOOTH_SCAN,Manifest.permission.BLUETOOTH_CONNECT}:
        new String[]{Manifest.permission.ACCESS_FINE_LOCATION};}
    void scanBle(){
        if(busy)return;
        for(String perm:blePermissions())if(checkSelfPermission(perm)!=PackageManager.PERMISSION_GRANTED){requestPermissions(blePermissions(),42);return;}
        stopScan();
        try{
            BluetoothManager m=(BluetoothManager)getSystemService(BLUETOOTH_SERVICE);
            if(m==null||m.getAdapter()==null||!m.getAdapter().isEnabled()){line("Включите Bluetooth и повторите поиск.");return;}
            scanner=m.getAdapter().getBluetoothLeScanner();if(scanner==null){line("Сканирование Bluetooth недоступно.");return;}
            found.clear();bleLabels.clear();showBle();
            callback=new ScanCallback(){
                public void onScanResult(int type,ScanResult r){
                    if(!scanning||mode()!=2||busy)return;
                    BluetoothDevice dev=r.getDevice();ScanRecord rec=r.getScanRecord();
                    String name=rec==null?null:rec.getDeviceName();if(name==null)name=dev.getName();
                    boolean uart=rec!=null&&rec.getServiceUuids()!=null&&rec.getServiceUuids().contains(new ParcelUuid(BlePort.SERVICE));
                    if((uart||(name!=null&&name.startsWith("MeshCore")))&&!found.contains(dev)){
                        found.add(dev);bleLabels.add((name==null?"MeshCore UART":name)+" ("+dev.getAddress()+")");showBle();}
                }
                public void onScanFailed(int e){scanning=false;line("Ошибка поиска Bluetooth: "+e);}
            };
            scanning=true;scanner.startScan(callback);line("Поиск BLE в течение 10 секунд…");
            final ScanCallback started=callback;
            handler.postDelayed(()->{if(scanning&&callback==started){stopScan();line("Устройств BLE найдено: "+found.size());}},10000);
        }catch(RuntimeException e){stopScan();line("Bluetooth: "+e.getMessage());}
    }
    void stopScan(){if(scanning&&scanner!=null&&callback!=null)try{scanner.stopScan(callback);}catch(RuntimeException ignored){}scanning=false;}
    @Override public void onRequestPermissionsResult(int req,String[] perms,int[] results){
        super.onRequestPermissionsResult(req,perms,results);
        if(req==42&&mode()==2){for(String p:blePermissions())if(checkSelfPermission(p)!=PackageManager.PERMISSION_GRANTED){line("Для поиска требуется разрешение Bluetooth (на старых Android — местоположение).");return;}scanBle();}
    }
    void ask(String op){
        if((op.equals("flash")||op.equals("reset"))&&displayName.toLowerCase(Locale.ROOT).endsWith(".mcfd")){
            line("Для дельты нажмите «Обновить прошивку».");return;
        }
        boolean flashing=!op.equals("info");
        if(mode()!=0){
            if(busy)return;
            if(!(op.equals("info")||op.equals("update")))return;
            int index=devices.getSelectedItemPosition();
            BluetoothDevice ble=mode()==2&&index>=0&&index<found.size()?found.get(index):null;
            String addr=host.getText().toString().trim();
            if(mode()==1&&addr.isEmpty()){host.setError("Укажите адрес платы");return;}
            if(mode()==2&&ble==null)return;
            Runnable start=()->wireless(ble,addr,flashing);
            if(flashing)new AlertDialog.Builder(this).setTitle("Обновить прошивку?").setMessage(displayName+"\n"+(ble==null?addr:ble.getAddress()))
                .setPositiveButton("Начать",(d,w)->start.run()).setNegativeButton("Отмена",null).show();else start.run();
            return;
        }
        int idx=devices.getSelectedItemPosition();if(busy||idx<0||idx>=drivers.size())return;
        UsbDevice target=drivers.get(idx).getDevice();
        if(flashing)new AlertDialog.Builder(this).setTitle(op.equals("flash") ? "Прошить через USB?" :
                op.equals("reset") ? "Создать новую ноду со сбросом данных?" : "Обновить прошивку?")
                .setMessage(displayName+"\n"+devices.getSelectedItem()+"\n\nУбедитесь, что файл подходит плате. Не отключайте USB."+ (op.equals("reset")?"\n\nБудут удалены прежний ключ, настройки, каналы, контакты и сопряжения обеих ролей.":""))
                .setNegativeButton("Отмена",null).setPositiveButton("Начать",(d,w)->permission(target,op)).show();
        else permission(target,op);
    }
    void permission(UsbDevice target,String op){
        busy=true;controls();report.setText("");getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        if(usb.hasPermission(target)){runOperation(target,op);return;}
        pending=target;requested=op;
        try {
            PendingIntent grant=PendingIntent.getBroadcast(this,target.getDeviceId(),new Intent(PERMISSION).setPackage(getPackageName()),
                PendingIntent.FLAG_UPDATE_CURRENT|PendingIntent.FLAG_IMMUTABLE);
            usb.requestPermission(target,grant);
        }catch(RuntimeException e){pending=null;line(e.toString());idle();}
    }
    byte[] readFile(Uri selected)throws IOException{
        try(InputStream in=getContentResolver().openInputStream(selected);ByteArrayOutputStream out=new ByteArrayOutputStream()){
            if(in==null)throw new IOException("Не удалось открыть файл.");
            byte[] b=new byte[16384];int n;
            while((n=in.read(b))!=-1){if(out.size()+n>8*1024*1024)throw new IOException("Файл больше 8 МиБ.");out.write(b,0,n);}
            return out.toByteArray();
        }
    }
    void wireless(BluetoothDevice dev,String address,boolean flashing){
        stopScan();busy=true;controls();report.setText("");getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        if(dev==null)getPreferences(0).edit().putString("host",address).apply();
        final Uri selected=file;
        final boolean delta=displayName.toLowerCase(Locale.ROOT).endsWith(".mcfd");
        new Thread(()->{
            try{
                byte[] image=flashing?readFile(selected):null;
                Ota.Port transport=dev==null?new TcpPort(address):new BlePort(this,dev);
                try(Closeable session=(Closeable)transport){Ota ota=new Ota(transport,this::line);if(flashing)ota.update(image,delta);else ota.info();}
            }catch(Exception e){line("Ошибка: "+e.getMessage());}
            finally{runOnUiThread(()->{if(!isDestroyed())idle();});}
        },"MeshCore-network").start();
    }
    void runOperation(UsbDevice target,String op){
        boolean flashing=!op.equals("info");
        final Uri selected=file;
        final boolean delta=displayName.toLowerCase(Locale.ROOT).endsWith(".mcfd");
        new Thread(()->{
            UsbDeviceConnection connection=null;
            try{
                byte[] bytes=flashing?readFile(selected):null;
                UsbSerialDriver driver=UsbSerialProber.getDefaultProber().probeDevice(target);
                if(driver==null||driver.getPorts().isEmpty())throw new IOException("Драйвер этого USB-устройства не найден.");
                connection=usb.openDevice(target);
                if(connection==null)throw new IOException("Не удалось открыть USB. Проверьте разрешение и кабель.");
                try(UsbSerialPort p=driver.getPorts().get(0)){
                    p.open(connection);p.setParameters(115200,8,UsbSerialPort.STOPBITS_1,UsbSerialPort.PARITY_NONE);
                    p.setRTS(false);p.setDTR(target.getVendorId()!=0x10c4);Thread.sleep(250);
                    EspInstall.Wire w=new EspInstall.Wire(){
                        public void dtr(boolean b)throws IOException{p.setDTR(b);} public void rts(boolean b)throws IOException{p.setRTS(b);}
                        public void write(byte[] data)throws IOException{p.write(data,10000);}
                        public int read(byte[] buffer,int ms)throws IOException{return p.read(buffer,ms);}
                    };
                    Ota session=new Ota(w,this::line);
                    if(op.equals("flash")||op.equals("reset")){
                        Ota.Device device=session.info();
                        new EspInstall(w,n->getAssets().open("esp/"+n),this::line,target.getVendorId()==0x303a&&target.getProductId()==0x1001)
                            .install(bytes,device,op.equals("reset"));
                    }else if(flashing)session.update(bytes,delta);else session.info();
                }
            }catch(Exception e){line("Ошибка: "+(e.getMessage()==null?e.toString():e.getMessage()));}
            finally{if(connection!=null)connection.close();runOnUiThread(()->{if(!isDestroyed())idle();});}
        },"MeshCore-USB").start();
    }
    void idle(){busy=false;getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);refresh();}
    @Override public void onBackPressed(){if(busy)Toast.makeText(this,"Дождитесь завершения операции.",Toast.LENGTH_SHORT).show();else super.onBackPressed();}
    @Override protected void onDestroy(){stopScan();handler.removeCallbacksAndMessages(null);unregisterReceiver(permissionReceiver);unregisterReceiver(devicesReceiver);super.onDestroy();}
}

