package ru.ommesh.otg;

import android.app.AlertDialog;
import android.view.LayoutInflater;
import android.view.View;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.RadioGroup;
import android.widget.ScrollView;
import android.widget.TextView;
import java.util.ArrayList;
import java.util.List;

final class ReleasePicker {
    interface Download {
        void select(FirmwareReleases.Asset asset);
    }

    private final MainActivity activity;
    private final FirmwareReleases.Target target;
    private final List<FirmwareReleases.Asset> assets;
    private final Download download;
    private final LayoutInflater inflater;
    private final ScrollView content;
    private final AlertDialog dialog;

    ReleasePicker(MainActivity activity,FirmwareReleases.Target target,List<FirmwareReleases.Asset> assets,
                  Download download) {
        this.activity=activity;this.target=target;this.assets=assets;this.download=download;
        inflater=activity.getLayoutInflater();
        content=(ScrollView)inflater.inflate(resource("release_catalog","layout"),null);
        dialog=new AlertDialog.Builder(activity).setTitle("Прошивки из GitHub").setView(content)
            .setNegativeButton("Закрыть",null).create();

        TextView model=view(content,"catalog_model");
        TextView context=view(content,"catalog_context");
        model.setText(target.uf2?target.model:target.model+" · "+FirmwareReleases.roleTitle(target.role));
        context.setText(target.uf2?"Выберите роль прошивки":target.installed>0?
            "Установлена сборка №"+target.installed:"Номер установленной сборки неизвестен");

        if(target.uf2){
            RadioGroup roles=view(content,"catalog_roles");
            View companion=view(content,"catalog_companion");
            View repeater=view(content,"catalog_repeater");
            companion.setEnabled(false);
            repeater.setEnabled(false);
            for(FirmwareReleases.Asset asset:assets){
                if(asset.role==FirmwareReleases.COMPANION)companion.setEnabled(true);
                if(asset.role==FirmwareReleases.REPEATER)repeater.setEnabled(true);
            }
            roles.setVisibility(assets.isEmpty()?View.GONE:View.VISIBLE);
            roles.setOnCheckedChangeListener((group,id)->{
                if(id==companion.getId())render(FirmwareReleases.COMPANION);
                if(id==repeater.getId())render(FirmwareReleases.REPEATER);
            });
        }
        render(target.role);
    }

    AlertDialog show() {
        dialog.show();
        return dialog;
    }

    private void render(int role) {
        List<FirmwareReleases.Asset> updates=new ArrayList<>();
        List<FirmwareReleases.Asset> other=new ArrayList<>();
        for(FirmwareReleases.Asset asset:assets){
            if(asset.role!=role)continue;
            if(target.updateIssue(asset).isEmpty())updates.add(asset);
            else other.add(asset);
        }

        FirmwareReleases.Asset recommended=FirmwareReleases.recommended(updates);
        View card=view(content,"catalog_recommended");
        TextView message=view(content,"catalog_message");
        card.setVisibility(recommended==null?View.GONE:View.VISIBLE);
        if(recommended!=null){
            text(content,"catalog_version",recommended.tag);
            text(content,"catalog_details",details(recommended));
            view(content,"catalog_preview").setVisibility(recommended.prerelease?View.VISIBLE:View.GONE);
            Button button=view(content,"catalog_download");
            button.setText("Скачать "+extension(recommended));
            button.setOnClickListener(v->choose(recommended));
            updates.remove(recommended);
        }else{
            message.setText(noRecommendation(role));
        }
        message.setVisibility(recommended==null?View.VISIBLE:View.GONE);

        LinearLayout entries=view(content,"catalog_entries");
        Button more=view(content,"catalog_more");
        entries.removeAllViews();
        entries.setVisibility(View.GONE);
        int count=updates.size()+other.size();
        String expand="Другие файлы ("+count+")";
        more.setVisibility(count==0?View.GONE:View.VISIBLE);
        more.setText(expand);
        more.setOnClickListener(v->{
            boolean open=entries.getVisibility()!=View.VISIBLE;
            if(open&&entries.getChildCount()==0){
                addRows(entries,updates,"Другие подходящие выпуски");
                addRows(entries,other,"Архив и файлы для ручной установки");
            }
            entries.setVisibility(open?View.VISIBLE:View.GONE);
            more.setText(open?"Скрыть другие файлы":expand);
        });
    }

    private String noRecommendation(int role) {
        if(assets.isEmpty())return "В последних выпусках GitHub нет файлов для этой модели и роли.";
        if(target.uf2)return role==0?"Компаньон подключается к приложению. Репитер передаёт сообщения между узлами.":
            "В каталоге нет .uf2 для выбранной роли.";
        if(target.installed==0)return "Проверьте номер сборки на плате перед выбором файла.";
        boolean allOlderOrInstalled=true;
        for(FirmwareReleases.Asset asset:assets){
            if(asset.build==0||asset.build>target.installed){allOlderOrInstalled=false;break;}
        }
        if(allOlderOrInstalled)return "В каталоге нет сборки новее №"+target.installed+".";
        String message="В каталоге нет подходящего обновления для сборки №"+target.installed+".";
        if(target.deltaOnly)message+=" Для полной прошивки выберите .uf2 в режиме «USB OTG» или «"
            +MainActivity.NRF52_BOOTLOADER_MODE+"».";
        return message;
    }

    private void addRows(LinearLayout parent,List<FirmwareReleases.Asset> list,String title) {
        if(list.isEmpty())return;
        TextView heading=new TextView(activity);
        heading.setTextAppearance(resource("FieldLabel","style"));
        heading.setText(title);
        heading.setPadding(0,activity.dp(12),0,activity.dp(6));
        parent.addView(heading);
        for(FirmwareReleases.Asset asset:list){
            View row=inflater.inflate(resource("release_row","layout"),parent,false);
            LinearLayout.LayoutParams params=(LinearLayout.LayoutParams)row.getLayoutParams();
            params.bottomMargin=activity.dp(6);
            text(row,"release_version",asset.tag);
            text(row,"release_details",details(asset)+(asset.prerelease?" · Предварительный":""));
            String issue=target.updateIssue(asset);
            if(!issue.isEmpty()){
                TextView note=view(row,"release_note");
                note.setText(issue);
                note.setVisibility(View.VISIBLE);
            }
            row.setOnClickListener(v->choose(asset));
            parent.addView(row);
        }
    }

    private void choose(FirmwareReleases.Asset asset) {
        dialog.dismiss();
        download.select(asset);
    }

    private static String extension(FirmwareReleases.Asset asset) {
        return "."+asset.format.name().toLowerCase(java.util.Locale.ROOT);
    }

    private static String details(FirmwareReleases.Asset asset) {
        String kind=asset.format==FirmwareFormat.MCFD?"Дельта с №"+asset.base:
            asset.format==FirmwareFormat.UF2?"Образ для загрузчика":"Полный образ";
        String size=asset.size<1024?asset.size+" Б":Math.round(asset.size/1024.0)+" КиБ";
        return kind+" · "+extension(asset)+" · "+size;
    }

    private int resource(String name,String type) {
        return activity.getResources().getIdentifier(name,type,activity.getPackageName());
    }

    private <T extends View> T view(View parent,String id) {
        return parent.findViewById(resource(id,"id"));
    }

    private void text(View parent,String id,String value) {
        TextView label=view(parent,id);
        label.setText(value);
    }
}
