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
import android.text.Editable;
import android.text.TextUtils;
import android.text.TextWatcher;
import android.view.*;
import android.widget.*;
import com.hoho.android.usbserial.driver.*;
import java.io.*;
import java.util.*;

public class MainActivity extends Activity {
    static final String PERMISSION="ru.ommesh.otg.USB_PERMISSION";
    static final String BLUETOOTH_UPDATE_MODE="Обновление по Bluetooth";
    static final String NRF52_BOOTLOADER_MODE="Загрузчик nRF52 (UF2)";
    static final int SELECT_FIRMWARE=1, EXPORT_DFU=2, CHECK_UF2_DRIVE=3, INSTALL_UF2=4, CATALOG_UF2=5;
    private static final int ENABLE_BLUETOOTH_FOR_SCAN = 6;
    private static final int LOG_STORAGE_PERMISSION = 43;
    UsbManager usb;
    Spinner devices;
    TextView fileName, fileHint, report, previousStatus, status, deviceLabel, deviceDetails, modelLabel;
    JournalPanel journal;
    CheckBox saveLogs;
    Button scan, selectFile, clearFile, info, update, releases, offline;
    final List<UsbSerialDriver> drivers=new ArrayList<>();
    boolean usbDiskPresent;
    Uf2Drive uf2Drive;
    int usbDiskGeneration;
    String checkedEndpoint;
    Ota.Device checkedDevice;
    String lastMessage="";
    private String reportFile = "";
    Uri file;
    UsbDevice pending;
    String requested="info";
    boolean busy, scanning;
    int activeMode;
    Spinner method, dfuModel;
    EditText host;
    ProgressBar progress;
    View operationProgress;
    TextView progressStage, progressPercent;
    boolean trackingProgress;
    Button flashUsb, resetNode, exportDfu;
    LegacyPackage pendingExport;
    TextView guide;
    BluetoothLeScanner scanner;
    ScanCallback callback;
    final Handler handler=new Handler(Looper.getMainLooper());
    final ArrayList<BleTarget> found=new ArrayList<>();
    final DfuBoard[] dfuBoards=DfuBoard.values();
    String dfuAddress;

    static final class BleTarget {
        final BluetoothDevice device;
        final String name;
        final DfuBoard board;

        BleTarget(BluetoothDevice device,String name){
            this.device=device;
            this.name=name;
            this.board=DfuBoard.fromAdvertisement(name);
        }

        String label(){
            return name+"\n"+device.getAddress();
        }
    }

    int mode(){return method.getSelectedItemPosition();}
    final BroadcastReceiver permissionReceiver=new BroadcastReceiver(){
        public void onReceive(Context c,Intent event){
            if(!PERMISSION.equals(event.getAction())||pending==null)return;
            UsbDevice target=pending; pending=null;
            if(usb.hasPermission(target))runOperation(target,requested);
            else{line("Доступ к USB не предоставлен.");failProgress();idle();}
        }
    };
    final BroadcastReceiver devicesReceiver=new BroadcastReceiver(){
        public void onReceive(Context c,Intent event){
            UsbDevice changed=event.getParcelableExtra(UsbManager.EXTRA_DEVICE);
            if(isUsbDisk(changed)){
                uf2Drive=null;
                usbDiskGeneration++;
            }
            if(changed!=null&&UsbManager.ACTION_USB_DEVICE_DETACHED.equals(event.getAction())
                &&Objects.equals(checkedEndpoint,"usb:"+changed.getDeviceName())){
                checkedDevice=null;
            }
            if(!busy&&mode()==0)refresh();
            else if(pending!=null&&!usb.getDeviceList().containsKey(pending.getDeviceName())){
                pending=null;line("Устройство отключено.");failProgress();idle();
            }
        }
    };
    @Override public void onCreate(Bundle saved){
        super.onCreate(saved);
        usb=(UsbManager)getSystemService(USB_SERVICE);
        setContentView(getResources().getIdentifier("activity_main","layout",getPackageName()));
        bindScreen();
        if(saved!=null){
            method.setSelection(saved.getInt("mode",0));
            dfuModel.setSelection(saved.getInt("dfuModel",0));
            String uri=saved.getString("file");
            if(uri!=null)file=Uri.parse(uri);
        }
        showFile();
        activeMode=mode();
        clearIncompatibleFile();
        IntentFilter hotplug=new IntentFilter(UsbManager.ACTION_USB_DEVICE_ATTACHED);hotplug.addAction(UsbManager.ACTION_USB_DEVICE_DETACHED);
        if(Build.VERSION.SDK_INT>=33){
            registerReceiver(permissionReceiver,new IntentFilter(PERMISSION),Context.RECEIVER_NOT_EXPORTED);
            registerReceiver(devicesReceiver,hotplug,Context.RECEIVER_EXPORTED);
        }else{registerReceiver(permissionReceiver,new IntentFilter(PERMISSION));registerReceiver(devicesReceiver,hotplug);}
        method.setOnItemSelectedListener(new AdapterView.OnItemSelectedListener(){
            public void onNothingSelected(AdapterView<?> v){}
            public void onItemSelected(AdapterView<?> v,View item,int p,long id){
                if(p!=activeMode){
                    activeMode=p;
                    stopScan();
                    found.clear();
                    clearIncompatibleFile();
                    resetStatus();
                }
                showFile();
                refresh();
            }
        });
        devices.setOnItemSelectedListener(selectionListener());
        dfuModel.setOnItemSelectedListener(selectionListener());
        refresh();
    }

    int dp(int value){return Math.round(getResources().getDisplayMetrics().density*value);}

    <T extends View> T view(String id){
        return findViewById(getResources().getIdentifier(id,"id",getPackageName()));
    }

    ArrayAdapter<String> choices(List<String> labels){
        int row=getResources().getIdentifier("port_item","layout",getPackageName());
        return new ArrayAdapter<>(this,row,labels);
    }

    AdapterView.OnItemSelectedListener selectionListener(){
        return new AdapterView.OnItemSelectedListener(){
            public void onItemSelected(AdapterView<?> v,View item,int position,long id){controls();}
            public void onNothingSelected(AdapterView<?> v){controls();}
        };
    }

    void bindScreen(){
        method=view("method");devices=view("devices");dfuModel=view("dfu_model");
        method.setAdapter(choices(Arrays.asList("USB OTG","Wi-Fi / TCP",BLUETOOTH_UPDATE_MODE,NRF52_BOOTLOADER_MODE)));
        List<String> models=new ArrayList<>();
        models.add("Выберите модель");
        for(DfuBoard board:dfuBoards)models.add(board.title);
        dfuModel.setAdapter(choices(models));
        deviceLabel=view("device_label");deviceDetails=view("device_details");
        modelLabel=view("model_label");guide=view("guide");
        fileName=view("file_name");fileHint=view("file_hint");status=view("status");progress=view("progress");
        operationProgress=view("operation_progress");
        progressStage=view("progress_stage");progressPercent=view("progress_percent");
        previousStatus=view("previous_status");
        saveLogs = view("save_journal");
        saveLogs.setOnCheckedChangeListener((button, checked) -> {
            if (checked && !OperationLog.canSave(this)) {
                requestPermissions(new String[]{Manifest.permission.WRITE_EXTERNAL_STORAGE}, LOG_STORAGE_PERMISSION);
            }
        });
        host=view("host");
        host.setText(getPreferences(0).getString("host",""));
        host.addTextChangedListener(new TextWatcher(){
            public void beforeTextChanged(CharSequence s,int start,int count,int after){}
            public void onTextChanged(CharSequence s,int start,int before,int count){showDeviceDetails();}
            public void afterTextChanged(Editable text){}
        });
        scan=view("scan");
        scan.setOnClickListener(v->{
            if(mode()>=2)scanBle();
            else{
                refresh();
                if(saveLogs.isChecked())startReport("Поиск USB-устройств…");
                if(usbDiskPresent&&usesUf2Drive())line("Обнаружен USB-диск. Нажмите «Проверить устройство», чтобы прочитать данные загрузчика.");
                else line(drivers.isEmpty()?"USB-устройства не найдены.":"Найдено USB-устройств: "+drivers.size());
                saveJournal();
            }
        });
        view("help").setOnClickListener(v->showHelp());
        selectFile=view("select_file");
        selectFile.setOnClickListener(v->startActivityForResult(FirmwareFormat.picker(mode()),SELECT_FIRMWARE));
        clearFile=view("clear_file");
        clearFile.setOnClickListener(v->{
            file=null;
            showFile();controls();
            line("Выбор файла снят.");
        });
        releases=view("releases");releases.setOnClickListener(v->chooseRelease());
        offline=view("offline");offline.setOnClickListener(v->openLibrary());
        exportDfu=view("export_dfu");exportDfu.setOnClickListener(v->prepareExport());
        info=view("info");info.setOnClickListener(v->{
            if(usesUf2Drive())selectUf2Drive(CHECK_UF2_DRIVE);
            else ask("info");
        });
        update=view("update");update.setOnClickListener(v->ask("update"));
        flashUsb=view("flash_usb");flashUsb.setOnClickListener(v->ask("flash"));
        resetNode=view("reset_node");resetNode.setOnClickListener(v->ask("reset"));
        report=view("journal_report");
        journal=view("journal_panel");
    }

    void showHelp(){
        String[] instructions={"help_usb","help_wifi","help_ble","help_dfu"};
        int selected=mode();
        CharSequence guideText=getText(getResources().getIdentifier(instructions[selected],"string",getPackageName()));
        CharSequence journalText=getText(getResources().getIdentifier("help_journal","string",getPackageName()));
        CharSequence message;
        if(selected==3)message=TextUtils.concat(guideText,"\n\n",journalText);
        else{
            CharSequence formats=getText(getResources().getIdentifier("help_formats","string",getPackageName()));
            message=TextUtils.concat(guideText,"\n\n",formats,"\n\n",journalText);
        }
        new AlertDialog.Builder(this).setTitle(method.getSelectedItem().toString()).setMessage(message)
            .setPositiveButton("Закрыть",null).show();
    }
    void refresh(){
        int m=mode();
        guide.setText(m==3?"Откройте BLE OTA на плате. Если модель не определилась, выберите её вручную.":
            m==0?"Подключите плату по USB. Для UF2 переведите её в режим USB-загрузчика.":
            m==1?"Введите адрес платы в вашей Wi-Fi-сети. Порт по умолчанию 5000.":
            "Откройте окно подключения на плате и запустите поиск.");
        deviceLabel.setText(m==3?"Загрузчик":"Устройство");
        deviceLabel.setVisibility(m==1?View.GONE:View.VISIBLE);
        modelLabel.setVisibility(m==3?View.VISIBLE:View.GONE);
        dfuModel.setVisibility(m==3?View.VISIBLE:View.GONE);
        host.setVisibility(m==1?View.VISIBLE:View.GONE);devices.setVisibility(m==1?View.GONE:View.VISIBLE);scan.setVisibility(m==1?View.GONE:View.VISIBLE);
        exportDfu.setVisibility(m==3?View.VISIBLE:View.GONE);
        info.setVisibility(m==3?View.GONE:View.VISIBLE);
        scan.setText(m==3?"Найти загрузчик":m==2?"Найти Bluetooth-устройства":"Найти USB-устройства");
        if(m==1){controls();return;}
        if(m>=2){showBle();return;}
        int idx=devices.getSelectedItemPosition();String old=idx>=0&&idx<drivers.size()?drivers.get(idx).getDevice().getDeviceName():"";
        drivers.clear();drivers.addAll(UsbSerialProber.getDefaultProber().findAllDrivers(usb));
        usbDiskPresent=false;
        for(UsbDevice device:usb.getDeviceList().values()){
            if(isUsbDisk(device)){usbDiskPresent=true;break;}
        }
        List<String> labels=new ArrayList<>();int current=0;
        for(int k=0;k<drivers.size();k++){UsbDevice dev=drivers.get(k).getDevice();String name=dev.getProductName();
            String label=String.format(Locale.ROOT,"%s · %04X:%04X",name==null?"USB Serial":name,dev.getVendorId(),dev.getProductId());
            labels.add(drivers.size()>1?label+"\n"+dev.getDeviceName():label);
            if(old.equals(dev.getDeviceName()))current=k;
        }
        if(labels.isEmpty())labels.add(usbDiskPresent?"USB-диск подключён":"USB-устройство не найдено");
        devices.setAdapter(choices(labels));devices.setSelection(current);
        controls();
    }
    void controls(){
        int m=mode();
        boolean dfu=m==3;
        boolean uf2=isUf2();
        boolean usbUf2=m==0&&uf2;
        boolean checkDrive=usesUf2Drive();
        BleTarget target=selectedBleTarget();
        UsbDevice serial=selectedUsbDevice();
        boolean connected=m==1||(m>=2?target!=null:serial!=null&&!isUsbDisk(serial));
        DfuBoard board=null;
        if(dfu){
            String address=target==null?null:target.device.getAddress();
            if(dfuAddress!=null&&!dfuAddress.equals(address))dfuModel.setSelection(0);
            dfuAddress=address;
            DfuBoard detected=target==null?null:target.board;
            if(detected!=null && dfuModel.getSelectedItemPosition()!=detected.ordinal()+1)
                dfuModel.setSelection(detected.ordinal()+1);
            dfuModel.setEnabled(!busy&&detected==null);
            modelLabel.setText(detected==null?"Модель платы":"Модель распознана");
            board=selectedDfuBoard();
        }
        method.setEnabled(!busy);host.setEnabled(!busy);devices.setEnabled(!busy&&(m==0?!drivers.isEmpty():connected));
        scan.setEnabled(!busy);selectFile.setEnabled(!busy);
        clearFile.setEnabled(!busy);
        clearFile.setVisibility(file==null?View.GONE:View.VISIBLE);
        releases.setEnabled(!busy&&(dfu?board!=null:(connected||checkDrive)));
        offline.setEnabled(!busy);
        String mismatch=dfu?dfuFirmwareIssue(board):
            usbUf2&&uf2Drive!=null?dfuFirmwareIssue(uf2Drive.info.board):"";
        fileHint.setText(mismatch);
        fileHint.setVisibility(mismatch.isEmpty()?View.GONE:View.VISIBLE);
        boolean ready=!busy&&connected&&file!=null&&FirmwareFormat.allowed(displayName,m);
        exportDfu.setEnabled(!busy&&uf2);
        info.setEnabled(!busy&&(connected||checkDrive));
        update.setText(dfu?"Начать":usbUf2?"Прошить UF2 по USB":"Обновить прошивку");
        update.setEnabled(usbUf2?!busy&&mismatch.isEmpty():ready&&(!dfu||board!=null&&mismatch.isEmpty()));
        flashUsb.setVisibility(m==0&&!checkDrive?View.VISIBLE:View.GONE);
        resetNode.setVisibility(m==0&&!checkDrive?View.VISIBLE:View.GONE);
        flashUsb.setEnabled(ready&&!uf2);resetNode.setEnabled(ready&&!uf2);
        showDeviceDetails();
    }

    static boolean isUsbDisk(UsbDevice device){
        if(device==null)return false;
        if(device.getDeviceClass()==UsbConstants.USB_CLASS_MASS_STORAGE)return true;
        for(int i=0;i<device.getInterfaceCount();i++){
            if(device.getInterface(i).getInterfaceClass()==UsbConstants.USB_CLASS_MASS_STORAGE)return true;
        }
        return false;
    }

    UsbDevice selectedUsbDevice(){
        int position=devices.getSelectedItemPosition();
        return mode()==0&&position>=0&&position<drivers.size()?drivers.get(position).getDevice():null;
    }

    boolean usesUf2Drive(){
        if(mode()!=0)return false;
        UsbDevice selected=selectedUsbDevice();
        return isUf2()||(uf2Drive!=null||usbDiskPresent)&&(selected==null||isUsbDisk(selected));
    }

    String endpoint(){
        if(mode()==1)return "tcp:"+host.getText().toString().trim();
        UsbDevice usbDevice=selectedUsbDevice();
        if(usbDevice!=null)return "usb:"+usbDevice.getDeviceName();
        BleTarget ble=selectedBleTarget();
        return ble!=null?"ble:"+ble.device.getAddress():null;
    }

    void showDeviceDetails(){
        String details="";
        if(usesUf2Drive()&&uf2Drive!=null)details=uf2Drive.info.summary();
        else if(mode()!=3&&checkedDevice!=null&&Objects.equals(checkedEndpoint,endpoint()))
            details=checkedDevice.summary();
        deviceDetails.setText(details);
        deviceDetails.setVisibility(details.isEmpty()?View.GONE:View.VISIBLE);
    }

    BleTarget selectedBleTarget(){
        int index=devices.getSelectedItemPosition();
        return mode()>=2&&index>=0&&index<found.size()?found.get(index):null;
    }

    DfuBoard selectedDfuBoard(){
        BleTarget target=selectedBleTarget();
        if(target!=null&&target.board!=null)return target.board;
        int index=dfuModel.getSelectedItemPosition()-1;
        return index>=0&&index<dfuBoards.length?dfuBoards[index]:null;
    }

    String dfuFirmwareIssue(DfuBoard board){
        if(file==null)return "";
        if(!isUf2())return "Для Bluetooth DFU выберите файл .uf2.";
        DfuBoard firmware=DfuBoard.fromFirmwareName(displayName);
        if(board!=null&&firmware!=null&&firmware!=board)
            return "Выбран файл для "+firmware.title+". Нужен UF2 для "+board.title+".";
        return "";
    }
    String displayName="";
    String nameOf(Uri source){
        if("file".equals(source.getScheme())){
            String name=source.getLastPathSegment();
            return name==null?"Выбранный файл":name;
        }
        try(Cursor c=getContentResolver().query(source,new String[]{OpenableColumns.DISPLAY_NAME},null,null,null)){
            if(c!=null&&c.moveToFirst()&&c.getString(0)!=null)return c.getString(0);
        }catch(RuntimeException ignored){}
        return "Выбранный файл";
    }
    void clearIncompatibleFile(){
        if(file!=null&&!FirmwareFormat.allowed(displayName,mode())){
            file=null;
            showFile();
        }
    }
    void showFile(){
        displayName=file==null?"":nameOf(file);
        if(file!=null)fileName.setText(displayName);
        else fileName.setText("Выберите файл "+FirmwareFormat.extensions(mode()));
    }
    boolean isUf2(){return FirmwareFormat.fromName(displayName)==FirmwareFormat.UF2;}
    void prepareExport(){
        final Uri source=file;
        final String name=displayName;
        busy=true;controls();
        beginProgress("Подготовка DFU-пакета");
        new Thread(()->{
            try{
                LegacyPackage pkg=LegacyPackage.fromUf2(readFile(source));
                runOnUiThread(()->{
                    if(isDestroyed())return;
                    pendingExport=pkg;
                    Intent out=new Intent(Intent.ACTION_CREATE_DOCUMENT).setType("application/zip");
                    out.addCategory(Intent.CATEGORY_OPENABLE);
                    out.putExtra(Intent.EXTRA_TITLE,name.substring(0,name.length()-4)+"-dfu.zip");
                    try{startActivityForResult(out,EXPORT_DFU);}catch(RuntimeException ex){pendingExport=null;line(ex.toString());failProgress();idle();}
                });
            }catch(Exception ex){line("Ошибка: "+ex.getMessage());runOnUiThread(()->{if(!isDestroyed()){failProgress();idle();}});}
        },"DFU-package").start();
    }
    @Override protected void onActivityResult(int request,int result,Intent data){
        super.onActivityResult(request,result,data);
        if (request == ENABLE_BLUETOOTH_FOR_SCAN) {
            if (result == RESULT_OK && mode() >= 2) scanBle();
            return;
        }
        if(request==CHECK_UF2_DRIVE||request==INSTALL_UF2||request==CATALOG_UF2){
            if(result==RESULT_OK&&data!=null&&data.getData()!=null&&mode()==0)inspectUf2Drive(data.getData(),request);
            else idle();
            return;
        }
        if(request==EXPORT_DFU){
            final LegacyPackage pkg=pendingExport;pendingExport=null;
            if(result!=RESULT_OK||data==null||data.getData()==null){clearProgress();idle();return;}
            final Uri dest=data.getData(), source=file;
            busy=true;controls();
            beginProgress("Сохранение DFU-пакета");
            new Thread(()->{
                try{
                    LegacyPackage ready=pkg!=null?pkg:LegacyPackage.fromUf2(readFile(source));
                    try(OutputStream out=getContentResolver().openOutputStream(dest,"wt")){
                        if(out==null)throw new IOException("Не удалось открыть файл для записи.");
                        ready.save(out);
                    }
                    line("DFU-пакет сохранён.");
                    runOnUiThread(()->{if(!isDestroyed())finishProgress("DFU-пакет сохранён");});
                }catch(Exception e){
                    line("Ошибка сохранения: "+e.getMessage());
                    runOnUiThread(()->{if(!isDestroyed())failProgress();});
                }
                finally{runOnUiThread(()->{if(!isDestroyed())idle();});}
            },"DFU-save").start();
            return;
        }
        if(request==SELECT_FIRMWARE&&result==RESULT_OK&&data!=null&&data.getData()!=null){
            Uri selected=data.getData();
            if(!FirmwareFormat.allowed(nameOf(selected),mode())){
                line("Выберите прошивку с расширением "+FirmwareFormat.extensions(mode())+".");
                return;
            }
            file=selected;
            showFile();
            clearProgress();
            controls();
        }
    }
    @Override protected void onSaveInstanceState(Bundle b){
        super.onSaveInstanceState(b);
        b.putInt("mode",mode());
        b.putInt("dfuModel",dfuAddress==null?dfuModel.getSelectedItemPosition():0);
        if(file!=null)b.putString("file",file.toString());
    }

    void startReport(String message){
        report.setText("");
        reportFile = file == null ? "" : displayName;
        resetStatus();
        line(message);
        if(journal.isExpanded())journal.scrollToLatest();
    }

    void resetStatus(){
        clearProgress();
        lastMessage="";
        previousStatus.setText("");
        previousStatus.setVisibility(View.GONE);
        status.setText("Готово к работе");
    }

    void clearProgress(){
        trackingProgress=false;
        operationProgress.setVisibility(View.GONE);
    }

    void beginProgress(String stage){
        trackingProgress=true;
        showProgress(stage,0);
        operationProgress.setVisibility(View.VISIBLE);
    }

    void showProgress(String stage,int percent){
        progressStage.setText(stage);
        progressPercent.setText(percent+" %");
        progress.setProgress(percent);
    }

    Ota.Progress reportProgress(){
        return new Ota.Progress(){
            int lastPercent=0;
            String lastStage="";

            public void update(String stage,int completed,int total){
                int percent=total<=0?0:(int)Math.min(99,100L*completed/total);
                // A retry can report fewer accepted units. Keep the last confirmed display.
                final int confirmed=Math.max(lastPercent,percent);
                if(confirmed==lastPercent&&stage.equals(lastStage))return;
                lastPercent=confirmed;
                lastStage=stage;
                runOnUiThread(()->{
                    if(!isDestroyed()&&trackingProgress)showProgress(stage,confirmed);
                });
            }
        };
    }

    void finishProgress(String stage){
        if(!trackingProgress)return;
        trackingProgress=false;
        showProgress(stage,100);
    }

    void failProgress(){
        if(!trackingProgress)return;
        trackingProgress=false;
        progressStage.setText("Ошибка · "+progressStage.getText());
    }

    void line(String message){
        runOnUiThread(()->{
            if(isDestroyed())return;
            previousStatus.setText(lastMessage);
            previousStatus.setVisibility(lastMessage.isEmpty()?View.GONE:View.VISIBLE);
            status.setText(message);
            lastMessage=message;
            boolean followLatest=journal.followsLatest();
            report.append(message+"\n");
            if(followLatest)journal.scrollToLatest();
        });
    }

    void saveJournal() {
        if (saveLogs == null || !saveLogs.isChecked() || lastMessage.isEmpty() || !OperationLog.canSave(this)) return;
        StringBuilder snapshot = new StringBuilder(String.valueOf(method.getSelectedItem())).append('\n');
        if (!reportFile.isEmpty()) snapshot.append("Файл · ").append(reportFile).append('\n');
        if (operationProgress.getVisibility() == View.VISIBLE) {
            snapshot.append(progressStage.getText()).append(' ').append(progressPercent.getText()).append('\n');
        }
        snapshot.append('\n').append(report.getText());
        String transcript = snapshot.toString();
        Context context = getApplicationContext();
        new Thread(() -> {
            String message;
            try {
                OperationLog.save(context, transcript);
                message = "Журнал сохранён в Загрузки";
            } catch (IOException | RuntimeException e) {
                message = "Не удалось сохранить журнал в Загрузки";
            }
            String result = message;
            runOnUiThread(() -> Toast.makeText(context, result, Toast.LENGTH_SHORT).show());
        }, "Save-operation-log").start();
    }

    void showBle(){
        int previous=Math.max(devices.getSelectedItemPosition(),0);
        List<String> labels=new ArrayList<>();
        for(BleTarget target:found)labels.add(target.label());
        if(labels.isEmpty())labels.add(mode()==3?"Загрузчик не найден":"Устройство не выбрано");
        devices.setAdapter(choices(labels));
        devices.setSelection(Math.min(previous,labels.size()-1));controls();
    }
    String[] blePermissions(){return Build.VERSION.SDK_INT>=31?
        new String[]{Manifest.permission.BLUETOOTH_SCAN,Manifest.permission.BLUETOOTH_CONNECT}:
        new String[]{Manifest.permission.ACCESS_FINE_LOCATION};}
    void scanBle(){
        if(busy)return;
        for(String perm:blePermissions())if(checkSelfPermission(perm)!=PackageManager.PERMISSION_GRANTED){requestPermissions(blePermissions(),42);return;}
        stopScan();
        clearProgress();
        if(saveLogs.isChecked())startReport("Поиск устройств Bluetooth…");
        try{
            BluetoothManager manager = getSystemService(BluetoothManager.class);
            BluetoothAdapter adapter = manager == null ? null : manager.getAdapter();
            if (adapter == null) {
                line("Адаптер Bluetooth недоступен на этом устройстве.");
                saveJournal();
                return;
            }
            if (!adapter.isEnabled()) {
                line("Bluetooth выключен. Включите его для поиска.");
                try {
                    startActivityForResult(new Intent(BluetoothAdapter.ACTION_REQUEST_ENABLE), ENABLE_BLUETOOTH_FOR_SCAN);
                } catch (ActivityNotFoundException e) {
                    line("Включите Bluetooth в настройках телефона и повторите поиск.");
                }
                saveJournal();
                return;
            }
            scanner=adapter.getBluetoothLeScanner();if(scanner==null){line("Сканирование Bluetooth недоступно.");saveJournal();return;}
            found.clear();showBle();
            callback=new ScanCallback(){
                public void onScanResult(int type,ScanResult r){
                    if(!scanning||callback!=this||mode()<2||busy)return;
                    BluetoothDevice dev=r.getDevice();ScanRecord rec=r.getScanRecord();
                    String name=rec==null?null:rec.getDeviceName();if(name==null)name=dev.getName();
                    boolean uart=rec!=null&&rec.getServiceUuids()!=null&&rec.getServiceUuids().contains(new ParcelUuid(BlePort.SERVICE));
                    String lower=name==null?"":name.toLowerCase(Locale.ROOT);
                    boolean legacy=rec!=null&&rec.getServiceUuids()!=null&&rec.getServiceUuids().contains(new ParcelUuid(BlePort.DFU));
                    boolean candidate=mode()==3?(legacy||lower.contains("dfu")||DfuBoard.fromAdvertisement(name)!=null):
                        (uart||(name!=null&&name.startsWith("MeshCore")));
                    if(candidate)recordBleTarget(dev,name);
                }
                public void onScanFailed(int e){
                    if(callback!=this)return;
                    scanning=false;
                    controls();
                    line("Ошибка поиска Bluetooth: "+e);
                    saveJournal();
                }
            };
            final boolean bootloader=mode()==3;
            final int seconds=bootloader?20:10;
            scanning=true;
            if(bootloader)scanner.startScan(null,new ScanSettings.Builder().setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY).build(),callback);
            else scanner.startScan(callback);
            controls();
            line("Поиск BLE в течение "+seconds+" секунд…");
            final ScanCallback started=callback;
            handler.postDelayed(()->{if(scanning&&callback==started){stopScan();line("Устройств BLE найдено: "+found.size());
                if(bootloader&&found.isEmpty())line("Откройте BLE OTA на плате и нажмите «Найти загрузчик» ещё раз.");
                saveJournal();}},seconds*1000L);
        }catch(RuntimeException e){stopScan();line("Bluetooth: "+e.getMessage());saveJournal();}
    }
    void recordBleTarget(BluetoothDevice device,String name){
        for(int i=0;i<found.size();i++){
            BleTarget prior=found.get(i);
            if(prior.device.equals(device)){
                // The service UUID and the model name can arrive in different scan results.
                if(name!=null && prior.board==null && DfuBoard.fromAdvertisement(name)!=null){
                    found.set(i,new BleTarget(device,name));
                    showBle();
                }
                return;
            }
        }
        found.add(new BleTarget(device,name==null?(mode()==3?"Nordic DFU":"MeshCore UART"):name));
        showBle();
    }
    void stopScan(){
        if(scanning&&scanner!=null&&callback!=null)try{scanner.stopScan(callback);}catch(RuntimeException ignored){}
        scanning=false;
    }
    @Override public void onRequestPermissionsResult(int req,String[] perms,int[] results){
        super.onRequestPermissionsResult(req,perms,results);
        if(req==LOG_STORAGE_PERMISSION){
            if(!OperationLog.canSave(this)){
                saveLogs.setChecked(false);
                line("Разрешите доступ к памяти, чтобы сохранять журнал в Загрузки.");
            }
            return;
        }
        if(req==42&&mode()>=2){for(String p:blePermissions())if(checkSelfPermission(p)!=PackageManager.PERMISSION_GRANTED){line("Для поиска требуется разрешение Bluetooth (на старых Android — местоположение).");return;}scanBle();}
    }
    void ask(String op){
        if(busy)return;
        boolean flashing=!(op.equals("info")||op.equals("catalog"));
        if(flashing&&(file==null||!FirmwareFormat.allowed(displayName,mode())))return;
        FirmwareFormat format=FirmwareFormat.fromName(displayName);
        if((op.equals("flash")||op.equals("reset"))&&format==FirmwareFormat.MCFD){
            line("Для дельты нажмите «Обновить прошивку».");return;
        }
        boolean merged = flashing && EspInstall.isMergedName(displayName);
        boolean direct = flashing && EspInstall.isDirectName(displayName);
        if (direct && op.equals("update")) {
            line("Для этого образа ESP выберите USB и нажмите «Прошить через USB».");
            return;
        }
        if (direct && !merged && op.equals("reset")) {
            line("Для полной очистки Heltec V3 выберите *-merged.bin для своей модели.");
            return;
        }
        if(mode()==3){
            askDfu();
            return;
        }
        if(mode()==0&&format==FirmwareFormat.UF2&&flashing){
            if(op.equals("update"))selectUf2Drive(INSTALL_UF2);
            return;
        }
        if(mode()!=0){
            if(!(op.equals("info")||op.equals("update")||op.equals("catalog")))return;
            BleTarget target=selectedBleTarget();
            BluetoothDevice ble=target==null?null:target.device;
            String addr=host.getText().toString().trim();
            if(mode()==1&&addr.isEmpty()){host.setError("Укажите адрес платы");return;}
            if(mode()==2&&ble==null)return;
            Runnable start=()->wireless(ble,addr,op);
            if(flashing)new AlertDialog.Builder(this).setTitle("Обновить прошивку?").setMessage(displayName+"\n"+(ble==null?addr:ble.getAddress()))
                .setPositiveButton("Начать",(d,w)->start.run()).setNegativeButton("Отмена",null).show();else start.run();
            return;
        }
        UsbDevice target=selectedUsbDevice();
        if(target==null||isUsbDisk(target))return;
        if (!flashing) {
            permission(target, op);
            return;
        }
        boolean reset = op.equals("reset");
        String message = displayName + "\n" + devices.getSelectedItem()
            + "\n\nСверьте модель платы и файл. Оставьте USB подключённым до окончания записи.";
        if (reset && format == FirmwareFormat.MCFW) {
            byte[] header = new byte[7];
            try (InputStream input = getContentResolver().openInputStream(file)) {
                Ota.need(input != null, "Не удалось открыть .mcfw.");
                new DataInputStream(input).readFully(header);
                message += "\n\n" + EspInstall.factoryDescription(header);
            } catch (IOException e) {
                line(e.getMessage() == null ? "Не удалось прочитать заголовок .mcfw." : e.getMessage());
                return;
            }
            message += "\n\nВся флеш-память будет очищена. Ключ, настройки и данные будут удалены.";
        } else if (merged) {
            message += "\n\nПолный образ содержит загрузчик и таблицу разделов.";
            message += reset ? " Вся флеш-память будет очищена перед записью."
                : " Образ перезапишет данные в занимаемой области флеш-памяти.";
        } else if (reset) {
            message += "\n\nБудут удалены прежний ключ, настройки, каналы, контакты и сопряжения обеих ролей.";
        }
        String title = op.equals("flash") ? "Прошить через USB?"
            : reset ? "Создать новую ноду со сбросом данных?" : "Обновить прошивку?";
        new AlertDialog.Builder(this).setTitle(title).setMessage(message)
            .setNegativeButton("Отмена", null).setPositiveButton("Начать", (d,w)->permission(target,op)).show();
    }
    void askDfu(){
        BleTarget target=selectedBleTarget();
        DfuBoard board=selectedDfuBoard();
        if(busy||!isUf2()||target==null||board==null||!dfuFirmwareIssue(board).isEmpty())return;
        stopScan();
        String message=board.title+"\n"+target.label()+"\n\n"+displayName
            +"\n\nПроверьте модель и роль прошивки. Держите плату рядом с телефоном.";
        if(board==DfuBoard.PRO_MICRO){
            message+="\n\nИспользуйте однобанковый загрузчик. Двухбанковая запись может затронуть контакты.";
        }
        new AlertDialog.Builder(this).setTitle("Установить UF2 по Bluetooth?")
            .setMessage(message).setPositiveButton("Начать",(d,w)->sendDfu(target.device))
            .setNegativeButton("Отмена",null).show();
    }

    void selectUf2Drive(int request){
        if(busy||mode()!=0)return;
        if(request!=CHECK_UF2_DRIVE&&uf2Drive!=null){
            inspectUf2Drive(uf2Drive.tree,request);
            return;
        }
        busy=true;controls();
        line("В системном меню откройте USB-диск платы и нажмите «Использовать эту папку».");
        Intent picker=new Intent(Intent.ACTION_OPEN_DOCUMENT_TREE)
            .putExtra(Intent.EXTRA_LOCAL_ONLY,true)
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION|Intent.FLAG_GRANT_WRITE_URI_PERMISSION);
        try{startActivityForResult(picker,request);}
        catch(ActivityNotFoundException e){
            line("На телефоне недоступен системный выбор папки USB.");
            idle();
        }
    }

    void inspectUf2Drive(Uri tree,int request){
        busy=true;controls();startReport("Чтение данных с USB-диска…");
        int generation=usbDiskGeneration;
        new Thread(()->{
            try{
                Uf2Drive drive=Uf2Drive.inspect(getContentResolver(),tree);
                runOnUiThread(()->{
                    if(isDestroyed())return;
                    if(generation!=usbDiskGeneration){
                        line("USB-диск отключился. Подключите загрузчик и повторите проверку.");
                        idle();
                        return;
                    }
                    uf2Drive=drive;
                    line(drive.info.summary().replace('\n',' '));
                    idle();
                    if(request==INSTALL_UF2)confirmUsbUf2(drive);
                    if(request==CATALOG_UF2){
                        DfuBoard board=drive.info.board;
                        if(board!=null)loadReleases(FirmwareReleases.Target.uf2(board));
                        else line("Модель диска не определена. Выберите UF2 для вашей платы вручную.");
                    }
                });
            }catch(Exception e){
                line("USB UF2. "+e.getMessage());
                runOnUiThread(()->{if(!isDestroyed())idle();});
            }
        },"UF2-drive").start();
    }

    void confirmUsbUf2(Uf2Drive drive){
        if(mode()!=0||file==null||!isUf2())return;
        try{
            drive.info.checkFirmwareName(displayName);
        }catch(IOException e){
            line(e.getMessage());
            return;
        }
        String message=drive.info.summary()+"\n\n"+displayName
            +"\n\nСверьте модель платы и файл. Оставьте USB подключённым до окончания передачи.";
        new AlertDialog.Builder(this).setTitle("Прошить UF2 по USB?")
            .setMessage(message).setNegativeButton("Отмена",null)
            .setPositiveButton("Начать",(d,w)->{
                if(uf2Drive==drive)sendUsbUf2(drive);
                else line("Диск сменился. Снова выберите загрузчик.");
            }).show();
    }

    void sendUsbUf2(Uf2Drive drive){
        if(busy||mode()!=0||file==null||!isUf2())return;
        final Uri source=file;
        final String name=displayName;
        busy=true;controls();startReport("Запись UF2 на USB-диск…");
        beginProgress("Подготовка UF2");
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        new Thread(()->{
            Uri installed=null;
            try{
                drive.write(getContentResolver(),name,readFile(source),this::line,reportProgress());
                installed=source;
                line("UF2 записан. Дождитесь перезагрузки платы.");
            }catch(Exception e){line("USB UF2. "+e.getMessage());}
            finally{
                Uri applied=installed;
                runOnUiThread(()->{
                    if(isDestroyed())return;
                    if(applied!=null)uf2Drive=null;
                    finishOperation(null,null,false,applied);
                });
            }
        },"UF2-USB").start();
    }

    void permission(UsbDevice target,String op){
        busy=true;controls();startReport("Подключение по USB…");getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        if(!op.equals("info")&&!op.equals("catalog"))beginProgress("Подключение по USB");
        if(usb.hasPermission(target)){runOperation(target,op);return;}
        pending=target;requested=op;
        try {
            PendingIntent grant=PendingIntent.getBroadcast(this,target.getDeviceId(),new Intent(PERMISSION).setPackage(getPackageName()),
                PendingIntent.FLAG_UPDATE_CURRENT|PendingIntent.FLAG_IMMUTABLE);
            usb.requestPermission(target,grant);
        }catch(RuntimeException e){pending=null;line(e.toString());failProgress();idle();}
    }
    byte[] readFile(Uri selected)throws IOException{
        return readFile(selected, 8);
    }
    byte[] readFile(Uri selected, int limitMiB)throws IOException{
        try(InputStream in=getContentResolver().openInputStream(selected);ByteArrayOutputStream out=new ByteArrayOutputStream()){
            if(in==null)throw new IOException("Не удалось открыть файл.");
            byte[] b=new byte[16384];int n;
            while((n=in.read(b))!=-1){if(out.size()+n>limitMiB*1024*1024)throw new IOException("Файл больше "+limitMiB+" МиБ.");out.write(b,0,n);}
            return out.toByteArray();
        }
    }
    void sendDfu(BluetoothDevice device){
        stopScan();busy=true;controls();startReport("Подготовка UF2…");getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        beginProgress("Подготовка UF2");
        final Uri selected=file;
        new Thread(()->{
            boolean installed=false;
            try{
                LegacyPackage pkg=LegacyPackage.fromUf2(readFile(selected));
                line("Подготовлен образ приложения: "+pkg.image.length+" байт.");
                try(BlePort port=BlePort.open(this,device,true,this::line)){
                    LegacyDfu.send(port,pkg,this::line,reportProgress());
                    installed=true;
                }
            }catch(Exception e){line("Ошибка DFU: "+e.getMessage());}
            finally{finishOperation("ble:"+device.getAddress(),null,false,installed?selected:null);}
        },"nRF52-DFU").start();
    }
    void wireless(BluetoothDevice dev,String address,String op){
        boolean flashing=op.equals("update");
        stopScan();busy=true;controls();startReport("Подключение к плате…");getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        if(flashing)beginProgress("Подключение к плате");
        if(dev==null)getPreferences(0).edit().putString("host",address).apply();
        final Uri selected=file;
        final FirmwareFormat format=FirmwareFormat.fromName(displayName);
        final String targetEndpoint=dev==null?"tcp:"+address:"ble:"+dev.getAddress();
        new Thread(()->{
            Ota.Device detected=null;
            boolean installed=false;
            try{
                byte[] image=flashing?readOtaFirmware(selected,format):null;
                Ota.Port transport=dev==null?new TcpPort(address):BlePort.open(this,dev,false,this::line);
                try(Closeable session=(Closeable)transport){
                    Ota ota=new Ota(transport,this::line);
                    if(flashing){
                        ota.update(image,format==FirmwareFormat.MCFD,reportProgress());
                        installed=true;
                    }else detected=ota.info();
                }
            }catch(Exception e){line("Ошибка: "+e.getMessage());}
            finally{finishOperation(targetEndpoint,detected,op.equals("catalog"),installed?selected:null);}
        },"MeshCore-network").start();
    }
    void runOperation(UsbDevice target,String op){
        boolean flashing=!(op.equals("info")||op.equals("catalog"));
        final Uri selected=file;
        final FirmwareFormat format=FirmwareFormat.fromName(displayName);
        final boolean bootloader = op.equals("flash") || op.equals("reset");
        final boolean reset = op.equals("reset");
        final boolean merged = bootloader && EspInstall.isMergedName(displayName);
        final boolean direct = bootloader && EspInstall.isDirectName(displayName);
        new Thread(()->{
            UsbDeviceConnection connection=null;
            Ota.Device detected=null;
            boolean installed=false;
            try{
                byte[] bytes=flashing?(merged?readFile(selected,16):readOtaFirmware(selected,format)):null;
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
                    if(bootloader){
                        EspInstall installer = new EspInstall(w, getAssets()::open, this::line,
                            target.getVendorId() == 0x303a && target.getProductId() == 0x1001);
                        Ota.Progress progress = reportProgress();
                        if (reset && format == FirmwareFormat.MCFW) installer.installFactory(bytes, progress);
                        else if (merged) installer.installMerged(bytes, reset, progress);
                        else if (direct) installer.installStockV3(bytes, progress);
                        else {
                            Ota.Device device;
                            try { device = session.info(); }
                            catch (java.net.SocketTimeoutException e) {
                                throw new IOException("Плата не ответила на запрос MeshCore-RS. Для первой установки .mcfw "
                                    + "выберите «Новая нода со сбросом».", e);
                            }
                            installer.install(bytes, device, reset, progress);
                        }
                        installed=true;
                    }else if(flashing){
                        session.update(bytes,format==FirmwareFormat.MCFD,reportProgress());
                        installed=true;
                    }else detected=session.info();
                }
            }catch(Exception e){line("Ошибка: "+(e.getMessage()==null?e.toString():e.getMessage()));}
            finally{if(connection!=null)connection.close();finishOperation("usb:"+target.getDeviceName(),detected,op.equals("catalog"),installed?selected:null);}
        },"MeshCore-USB").start();
    }
    byte[] readOtaFirmware(Uri selected,FirmwareFormat format)throws IOException{
        Ota.need(selected!=null&&format!=null&&format!=FirmwareFormat.UF2,"Выберите файл .mcfw, .mcfd или .bin.");
        byte[] firmware=readFile(selected);
        if(format==FirmwareFormat.MCFW||format==FirmwareFormat.MCFD)
            Ota.need(firmware.length>=54&&firmware[0]=='M'&&firmware[1]=='C'&&firmware[2]=='F'&&firmware[3]=='W',
                "Файл не содержит заголовок прошивки MeshCore-RS.");
        return firmware;
    }
    void idle(){
        if (busy) saveJournal();
        reportFile = "";
        busy=false;getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);refresh();
    }
    void finishOperation(String targetEndpoint,Ota.Device detected,boolean catalog,Uri installedFile){
        runOnUiThread(()->{
            if(isDestroyed())return;
            if(detected!=null){
                checkedEndpoint=targetEndpoint;
                checkedDevice=detected;
            }
            if(installedFile!=null&&installedFile.equals(file)){
                file=null;
                showFile();
                if(Objects.equals(checkedEndpoint,targetEndpoint))checkedDevice=null;
            }
            if(installedFile!=null)finishProgress("Запись завершена");
            else failProgress();
            idle();
            if(catalog && detected!=null){
                if(!Objects.equals(targetEndpoint,endpoint())){
                    line("Устройство сменилось. Снова нажмите «Из GitHub».");return;
                }
                try{loadReleases(FirmwareReleases.Target.ota(detected));}
                catch(IOException e){line(e.getMessage());}
            }
        });
    }
    void chooseRelease(){
        if(busy)return;
        if(usesUf2Drive()){selectUf2Drive(CATALOG_UF2);return;}
        if(mode()!=3){ask("catalog");return;}
        DfuBoard board=selectedDfuBoard();
        if(board!=null)loadReleases(FirmwareReleases.Target.uf2(board));
    }
    void loadReleases(FirmwareReleases.Target target){
        String selectedEndpoint=endpoint();
        int diskGeneration=usbDiskGeneration;
        stopScan();busy=true;controls();line("Загрузка выпусков для "+target.model+"…");
        beginProgress("Загрузка списка выпусков");
        new Thread(()->{
            try{
                List<FirmwareReleases.Asset> assets=FirmwareReleases.load(target);
                runOnUiThread(()->{
                    if(isDestroyed())return;
                    clearProgress();
                    idle();
                    boolean changed=target.uf2?mode()==0&&usbDiskGeneration!=diskGeneration:
                        !Objects.equals(selectedEndpoint,endpoint());
                    if(changed){line("Устройство сменилось. Снова нажмите «Из GitHub».");return;}
                    new ReleasePicker(this,target,assets,this::downloadRelease).show();
                });
            }catch(Exception e){line("Каталог GitHub: "+e.getMessage());runOnUiThread(()->{if(!isDestroyed()){failProgress();idle();}});}
        },"Firmware-releases").start();
    }
    void downloadRelease(FirmwareReleases.Asset asset){
        busy=true;controls();line("Загрузка "+asset.name+"…");
        beginProgress("Подключение к GitHub");
        new Thread(()->{
            try{
                File downloaded=FirmwareLibrary.store(asset,firmwareDirectory(),reportProgress()).file;
                runOnUiThread(()->{
                    if(isDestroyed())return;
                    file=Uri.fromFile(downloaded);showFile();
                    finishProgress("Прошивка загружена");
                    line("Загружено "+asset.tag+". Файл выбран для прошивки.");idle();
                });
            }catch(Exception e){line("Ошибка загрузки: "+e.getMessage());runOnUiThread(()->{if(!isDestroyed()){failProgress();idle();}});}
        },"Firmware-download").start();
    }

    private File firmwareDirectory(){return new File(getFilesDir(),"firmware");}

    private interface LibraryResult<T>{void accept(T result);}

    private <T> void libraryWork(String caption,boolean transfer,java.util.concurrent.Callable<T> task,LibraryResult<T> result){
        if(busy)return;
        stopScan();busy=true;controls();startReport(caption);
        if(transfer)beginProgress(caption);
        new Thread(()->{
            try{
                T value=task.call();
                runOnUiThread(()->{
                    if(isDestroyed())return;
                    if(transfer)finishProgress("Набор сохранён");
                    idle();
                    result.accept(value);
                });
            }catch(Exception error){
                line("Библиотека прошивок. "+(error.getMessage()==null?error.toString():error.getMessage()));
                runOnUiThread(()->{if(!isDestroyed()){failProgress();idle();}});
            }
        },"Firmware-library").start();
    }

    void openLibrary(){
        int mode=mode();
        boolean disk=mode==0&&uf2Drive!=null&&usesUf2Drive();
        DfuBoard model=mode==3?selectedDfuBoard():disk?uf2Drive.info.board:null;
        int board=model==null?0:Arrays.asList(FirmwareReleases.BOARDS).indexOf(model.releaseName);
        Ota.Device device=mode!=3&&!disk&&Objects.equals(checkedEndpoint,endpoint())?checkedDevice:null;
        libraryWork("Чтение сохранённых прошивок…",false,
            ()->FirmwareLibrary.saved(firmwareDirectory(),mode,disk,Math.max(0,board),device),
            saved->FirmwareLibraryDialogs.showSaved(this,saved));
    }

    void loadLibraryVersions(){
        libraryWork("Загрузка списка выпусков…",false,FirmwareLibrary::versions,
            versions->FirmwareLibraryDialogs.showVersions(this,versions));
    }

    void downloadLibrary(List<FirmwareReleases.Asset> files){
        if(files.isEmpty())return;
        libraryWork("Загрузка офлайн-набора",true,
            ()->FirmwareLibrary.sync(files,firmwareDirectory(),reportProgress()),
            downloaded->line("Скачано: "+downloaded+". Уже в библиотеке: "+(files.size()-downloaded)+"."));
    }

    void chooseSavedFirmware(FirmwareLibrary.Entry entry){
        if(!entry.enabled)return;
        libraryWork("Проверка сохранённого файла…",false,()->FirmwareLibrary.verify(entry,firmwareDirectory()),saved->{
            file=Uri.fromFile(saved);showFile();controls();
            line("Выбрана сохранённая прошивка "+entry.file.getName()+".");
        });
    }

    @Override public void onBackPressed(){
        if(journal.isExpanded())journal.setExpanded(false);
        else if(busy)Toast.makeText(this,"Дождитесь завершения операции.",Toast.LENGTH_SHORT).show();
        else super.onBackPressed();
    }
    @Override protected void onDestroy(){
        stopScan();
        handler.removeCallbacksAndMessages(null);
        unregisterReceiver(permissionReceiver);unregisterReceiver(devicesReceiver);
        super.onDestroy();
    }
}

