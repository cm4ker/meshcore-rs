//! Where a node's firmware update stands, in the screens' words: the same [`FwState`] that `fw`
//! tells in English (see `meshcore::fwstate`), as a few words and a line under them.

use alloc::format;
use alloc::string::{String, ToString};

use meshcore::fwstate::{FwState, hm};

/// What the update page shows.
pub struct FwText {
    /// Where it stands, a few words ("Обновлений нет", "0.8.4 #47 готова").
    pub state: String,
    /// Why, or what comes next.
    pub detail: String,
    /// Bytes downloaded of the update, while it comes.
    pub progress: Option<(u32, u32)>,
}

/// `state` on the screen; `trial`: the new firmware is kept in this many seconds, and goes back to
/// that one on `fw rollback`. `app`: a companion, whose settings are the app's custom variables.
pub fn fw_text(state: &FwState, trial: Option<(u64, &str)>, app: bool) -> FwText {
    let text = |state: String, detail: String| FwText { state, detail, progress: None };
    if let Some((secs, prev)) = trial {
        let mins = secs.div_ceil(60).max(1);
        let back = if prev.is_empty() { String::new() } else { format!(", откат: {prev}") };
        return text("Пробный запуск".into(), format!("Оставлю через {mins} мин{back}"));
    }
    match state {
        FwState::Off => text("Выключено".into(), if app { "Включите fw_auto в настройках приложения" } else { "Включить: set fw.auto install" }.into()),
        FwState::NoRoot => {
            text("Нет ключа выпусков".into(), if app { "Задайте fw_root в настройках приложения" } else { "Задать: set fw.root" }.into())
        }
        FwState::NoSlot => text("Нет места для обновления".into(), "Обновить можно только по USB".into()),
        FwState::Waiting { at: 0 } => text("Ждёт проверки".into(), "Первая через 10 минут после включения".into()),
        FwState::Waiting { at } => text("Ждёт проверки".into(), format!("Проверю в {}", hm(*at))),
        FwState::Asking { whom } => text("Проверяю".into(), format!("Спрашиваю {whom}")),
        FwState::UpToDate { whom, at } => text("Обновлений нет".into(), format!("{whom}, {}", hm(*at))),
        FwState::Fetching { whom } => text(format!("{whom} загружает"), "Спрошу снова через минуту".into()),
        FwState::Downloading { release, from, done, total } => {
            FwText { state: format!("Загружается {release}"), detail: format!("от {from}"), progress: Some((*done, *total)) }
        }
        FwState::Ready { release, .. } => text(format!("{release} готова"), "Установка займёт около 10 секунд".into()),
        FwState::Held { release } => text(format!("{release} готова"), "Установлю, когда сосед докачает свою".into()),
        FwState::Skipped { release } => text("Обновлений нет".into(), format!("{release} откатили, жду следующую")),
        FwState::Installing { release } => text(format!("Устанавливаю {release}"), "Перезапуск, 1–2 минуты".into()),
        FwState::Failed { why, retry } => {
            let again = if *retry == 0 { String::new() } else { format!(". Попробую в {}", hm(*retry)) };
            text("Не удалось".into(), format!("{}{again}", reason(why)))
        }
    }
}

/// Why an update failed, as the core says it in English, in the screen's words.
pub fn reason(why: &str) -> String {
    // "<server> not reachable, <the rest>" on a repeater without a link to its server
    if let Some((server, rest)) = why.split_once(" not reachable, ") {
        return format!("{server} недоступен, {}", lower_first(&reason(rest)));
    }
    let fixed = match why {
        "not signed by the pinned root key" => "Подписано не тем ключом, что закреплён здесь",
        "not signed by the owner key" => "Не подписано ключом владельца",
        "made for another board" => "Сделано для другой платы",
        "older than this firmware" => "Старше этой прошивки",
        "made for another firmware than this one" => "Сделано не для этой прошивки",
        "unreadable" => "Не читается",
        "the update slot can't be written" => "Не пишется память для обновления",
        "refused" => "Отклонено",
        "a piece came short" => "Кусок пришёл неполным",
        "not an update file" => "Это не файл обновления",
        "a piece out of order" => "Куски пришли не по порядку",
        "pieces of the update are missing" => "Обновление пришло не целиком",
        "the update file can't be read" => "Файл обновления не читается",
        "the update file is cut short" => "Файл обновления оборван",
        "can't read this firmware's image" => "Не читается своя прошивка",
        "a dev release, and this device follows stable" => "Тестовый выпуск, а здесь стабильный канал",
        "the repeater is not a contact" => "Репитера нет в контактах",
        "no repeater heard directly; one with fw.serve on is needed" => "Рядом нет репитера с fw.serve on",
        "no repeater heard directly to ask" => "Рядом не слышно репитеров",
        "no neighbour hands updates on (fw.serve)" => "Никто из соседей не раздаёт обновления",
        "can't reach the neighbour" => "Сосед недоступен",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.into();
    }
    // the ones that name a neighbour
    let named: [(&str, fn(&str) -> String); 5] = [
        (" stopped answering", |n| format!("{n} перестал отвечать")),
        (" not heard lately", |n| format!("{n} давно не слышно")),
        (" has no updates now", |n| format!("У {n} сейчас нет обновлений")),
        (" could not send the update", |n| format!("{n} не смог отдать обновление")),
        (" is not a contact", |n| format!("{n} нет в контактах")),
    ];
    if let Some(n) = why.strip_prefix("no answer from ") {
        return format!("{n} не ответил");
    }
    if let Some(rest) = why.strip_prefix("the update is ")
        && let Some((kb, rest)) = rest.split_once(" KB, more than this node takes over LoRa (")
        && let Some((max, _)) = rest.split_once(" KB)")
    {
        return format!("Обновление {kb} КБ, по LoRa не больше {max} КБ: только USB или Bluetooth");
    }
    named
        .iter()
        .find_map(|(suffix, say)| why.strip_suffix(suffix).map(say))
        // a commit failure and the like: as the core says it
        .unwrap_or_else(|| why.to_string())
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_lowercase().chain(c).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_in_the_screens_words() {
        assert_eq!(reason("PKIO stopped answering"), "PKIO перестал отвечать");
        assert_eq!(reason("no answer from PKIO"), "PKIO не ответил");
        assert_eq!(
            reason("nw.ommesh.ru not reachable, no neighbour hands updates on (fw.serve)"),
            "nw.ommesh.ru недоступен, никто из соседей не раздаёт обновления"
        );
        assert_eq!(
            reason("the update is 180 KB, more than this node takes over LoRa (160 KB): install it over USB or BLE"),
            "Обновление 180 КБ, по LoRa не больше 160 КБ: только USB или Bluetooth"
        );
        assert_eq!(reason("OTA: commit failed: Flash"), "OTA: commit failed: Flash");
    }
}
