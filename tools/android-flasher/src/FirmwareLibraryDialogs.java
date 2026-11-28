package ru.ommesh.otg;

import android.app.AlertDialog;
import android.view.View;
import android.widget.AdapterView;
import android.widget.ArrayAdapter;
import android.widget.Button;
import android.widget.CheckBox;
import android.widget.LinearLayout;
import android.widget.ListView;
import android.widget.Spinner;
import android.widget.TextView;
import java.util.*;

final class FirmwareLibraryDialogs {
    static void showSaved(MainActivity app,FirmwareLibrary.Saved data){
        AlertDialog.Builder builder=new AlertDialog.Builder(app)
            .setTitle("Офлайн-библиотека · "+data.total)
            .setNeutralButton("Скачать набор…",(dialog,button)->app.loadLibraryVersions())
            .setNegativeButton("Закрыть",null);
        List<FirmwareLibrary.Entry> files=data.files;
        if(files.isEmpty()){
            builder.setMessage(data.total==0?
                "Скачайте набор заранее, чтобы прошивать без интернета.":
                "Нет сохранённых прошивок для выбранной платы или способа подключения.").show();
            return;
        }
        List<FirmwareLibrary.Entry> deltas=new ArrayList<>();
        for(FirmwareLibrary.Entry entry:files)if(entry.format==FirmwareFormat.MCFD)deltas.add(entry);

        LinearLayout content=new LinearLayout(app);
        content.setOrientation(LinearLayout.VERTICAL);
        CheckBox onlyDelta=new CheckBox(app);
        onlyDelta.setText("Только дельты .mcfd ("+deltas.size()+")");
        onlyDelta.setEnabled(!deltas.isEmpty());
        onlyDelta.setPadding(app.dp(16),0,app.dp(16),0);
        content.addView(onlyDelta);
        ListView list=new ListView(app);
        list.setChoiceMode(ListView.CHOICE_MODE_SINGLE);
        int height=Math.min(app.dp(345),app.getResources().getDisplayMetrics().heightPixels/2);
        content.addView(list,new LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT,height));

        FirmwareLibrary.Entry[] selected={files.get(0)};
        AlertDialog dialog=builder.setView(content).setPositiveButton("Выбрать",(window,button)->
            app.chooseSavedFirmware(selected[0])).show();
        Button use=dialog.getButton(AlertDialog.BUTTON_POSITIVE);
        list.setOnItemClickListener((parent,view,position,id)->{
            List<FirmwareLibrary.Entry> shown=onlyDelta.isChecked()?deltas:files;
            selected[0]=shown.get(position);
            use.setEnabled(selected[0].enabled);
        });
        Runnable refresh=()->{
            List<FirmwareLibrary.Entry> shown=onlyDelta.isChecked()?deltas:files;
            List<String> labels=new ArrayList<>();
            for(FirmwareLibrary.Entry entry:shown){
                String label=entry.label();
                if(!entry.issue.isEmpty())label+="\n"+(entry.enabled?"Только через USB-загрузчик · ":"")+entry.issue;
                labels.add(label);
            }
            int index=Math.max(0,shown.indexOf(selected[0]));
            selected[0]=shown.get(index);
            list.clearChoices();
            list.setAdapter(new ArrayAdapter<>(app,android.R.layout.simple_list_item_single_choice,labels));
            list.setItemChecked(index,true);
            list.setSelection(index);
            use.setEnabled(selected[0].enabled);
        };
        onlyDelta.setOnCheckedChangeListener((button,checked)->refresh.run());
        refresh.run();
    }

    static void showVersions(MainActivity app,List<FirmwareLibrary.Version> versions){
        if(versions.isEmpty()){
            new AlertDialog.Builder(app).setTitle("Скачать набор")
                .setMessage("В выпусках GitHub нет файлов для офлайн-набора.")
                .setPositiveButton("Закрыть",null).show();
            return;
        }
        LinearLayout content=new LinearLayout(app);
        content.setOrientation(LinearLayout.VERTICAL);
        content.setPadding(app.dp(22),app.dp(8),app.dp(22),app.dp(8));
        List<String> labels=new ArrayList<>();
        for(FirmwareLibrary.Version release:versions)
            labels.add(release.tag+(release.prerelease?" · предварительный":""));
        Spinner release=option(app,content,"Выпуск",labels);
        Spinner model=option(app,content,"Модель",Arrays.asList(FirmwareLibrary.MODELS));
        Spinner role=option(app,content,"Роль",Arrays.asList("Обе роли","Компаньон","Репитер"));
        int[] roles={0,FirmwareReleases.COMPANION,FirmwareReleases.REPEATER};
        TextView summary=new TextView(app);
        summary.setPadding(0,app.dp(10),0,app.dp(6));
        content.addView(summary);
        AlertDialog dialog=new AlertDialog.Builder(app)
            .setTitle("Скачать набор")
            .setView(content)
            .setNegativeButton("Назад",(window,button)->app.openLibrary())
            .setPositiveButton("Скачать",(window,button)->{
                FirmwareLibrary.Version selected=versions.get(release.getSelectedItemPosition());
                app.downloadLibrary(selected.select(model.getSelectedItemPosition(),roles[role.getSelectedItemPosition()]));
            }).show();
        Button download=dialog.getButton(AlertDialog.BUTTON_POSITIVE);
        Runnable update=()->{
            int versionIndex=release.getSelectedItemPosition(), boardIndex=model.getSelectedItemPosition(), roleIndex=role.getSelectedItemPosition();
            if(versionIndex<0||boardIndex<0||roleIndex<0){download.setEnabled(false);return;}
            List<FirmwareReleases.Asset> plan=versions.get(versionIndex).select(boardIndex,roles[roleIndex]);
            long bytes=0;
            for(FirmwareReleases.Asset item:plan)bytes+=item.size;
            summary.setText(plan.isEmpty()?"Для выбранной модели и роли файлов нет.":
                String.format(Locale.getDefault(),"Файлов: %d · %.1f МиБ",plan.size(),bytes/1048576.0));
            download.setEnabled(!plan.isEmpty());
        };
        AdapterView.OnItemSelectedListener listener=new AdapterView.OnItemSelectedListener(){
            public void onItemSelected(AdapterView<?> parent,View view,int position,long id){update.run();}
            public void onNothingSelected(AdapterView<?> parent){download.setEnabled(false);}
        };
        release.setOnItemSelectedListener(listener);
        model.setOnItemSelectedListener(listener);
        role.setOnItemSelectedListener(listener);
        update.run();
    }

    private static Spinner option(MainActivity app,LinearLayout content,String caption,List<String> labels){
        TextView title=new TextView(app);
        title.setText(caption);
        title.setPadding(0,app.dp(8),0,0);
        content.addView(title);
        Spinner selector=new Spinner(app);
        selector.setAdapter(app.choices(labels));
        selector.setContentDescription(caption);
        selector.setMinimumHeight(app.dp(48));
        content.addView(selector,new LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT,
            LinearLayout.LayoutParams.WRAP_CONTENT));
        return selector;
    }
}
