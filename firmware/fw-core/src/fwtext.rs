//! Where a node's firmware update stands, in the screens' compact words: the same [`FwState`]
//! that `fw` tells in English (see `meshcore::fwstate`), shortened to fit small displays cleanly.

use alloc::format;
use alloc::string::{String, ToString};

use meshcore::fwstate::{FwState, hm};

/// What the update page shows.
pub struct FwText {
    /// Short status ("Актуально", "0.5.2 готова", "Ошибка").
    pub state: String,
    /// Concise detail line under it.
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
        let detail = if prev.is_empty() { String::new() } else { format!("откат {prev}") };
        return text(format!("Пробный · {mins}м"), detail);
    }
    match state {
        FwState::Off => text("Авто выкл".into(), if app { "fw_auto = off" } else { "set fw.auto install" }.into()),
        FwState::NoRoot => text("Нет ключа".into(), if app { "нужен fw_root" } else { "set fw.root" }.into()),
        FwState::NoSlot => text("Нет слота".into(), "только USB".into()),
        FwState::Waiting { at: 0 } => text("Ожидание".into(), "через 10м".into()),
        FwState::Waiting { at } => text("Ожидание".into(), format!("в {}", hm(*at))),
        FwState::Asking { whom } => text("Проверка".into(), format!("{whom}...")),
        FwState::UpToDate { whom, at } => {
            let host = whom.strip_prefix("nw.").and_then(|s| s.split('.').next()).unwrap_or(whom);
            text("Актуально".into(), format!("{host} · {}", hm(*at)))
        }
        FwState::Fetching { whom } => text(format!("{whom} качает"), "повтор через 1м".into()),
        FwState::Downloading { release, from, done, total } => {
            FwText { state: release.to_string(), detail: format!("от {from}"), progress: Some((*done, *total)) }
        }
        FwState::Ready { release, .. } => text(format!("{release} готова"), String::new()),
        FwState::Held { release } => text(format!("{release} готова"), "ждет соседа".into()),
        FwState::Skipped { release } => text("Актуально".into(), format!("{release} откатили")),
        FwState::Installing { release } => text(format!("Установка {release}"), "перезапуск...".into()),
        FwState::Failed { why, retry } => {
            let again = if *retry == 0 { String::new() } else { format!(" · {}", hm(*retry)) };
            text("Ошибка".into(), format!("{}{again}", reason(why)))
        }
    }
}

/// Why an update failed, as the core says it in English, in compact screen words.
pub fn reason(why: &str) -> String {
    if let Some((server, rest)) = why.split_once(" not reachable, ") {
        let host = server.strip_prefix("nw.").and_then(|s| s.split('.').next()).unwrap_or(server);
        return format!("{host} недоступен: {}", reason(rest));
    }
    let fixed = match why {
        "not signed by the pinned root key" => "Чужой ключ",
        "not signed by the owner key" => "Нет ключа влад.",
        "made for another board" => "Другая плата",
        "older than this firmware" => "Старше текущей",
        "made for another firmware than this one" => "Другая прошивка",
        "unreadable" => "Не читается",
        "the update file is corrupted" => "Битый файл",
        "the update slot can't be written" => "Ошибка flash",
        "larger than this device's update slot" => "Нет слота",
        "an update is already waiting for restart" => "Ждет рестарта",
        "refused" => "Отклонено",
        "a piece came short" => "Короткий блок",
        "not an update file" => "Не файл OTA",
        "a piece out of order" => "Сбой порядка",
        "pieces of the update are missing" => "Нет части блоков",
        "the update file can't be read" => "Ошибка чтения",
        "the update file is cut short" => "Файл оборван",
        "can't read this firmware's image" => "Ошибка образа",
        "a dev release, and this device follows stable" => "Нужен канал dev",
        "the repeater is not a contact" => "Нет в контактах",
        "no repeater heard directly; one with fw.serve on is needed" => "Нет репит. fw.serve",
        "no repeater heard directly to ask" => "Нет репитеров",
        "no neighbour hands updates on (fw.serve)" => "Нет соседей OTA",
        "can't reach the neighbour" => "Сосед недоступен",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.into();
    }
    let named: [(&str, fn(&str) -> String); 5] = [
        (" stopped answering", |n| format!("{n}: нет ответа")),
        (" not heard lately", |n| format!("{n}: не слышно")),
        (" has no updates now", |n| format!("{n}: нет OTA")),
        (" could not send the update", |n| format!("{n}: сбой TX")),
        (" is not a contact", |n| format!("{n}: не контакт")),
    ];
    if let Some(n) = why.strip_prefix("no answer from ") {
        return format!("{n}: нет ответа");
    }
    if let Some(rest) = why.strip_prefix("the update is ")
        && let Some((kb, rest)) = rest.split_once(" KB, ")
    {
        let between = |r: &'_ str, from: &str, to: &str| r.strip_prefix(from)?.split_once(to).map(|(s, _)| s.to_string());
        if let Some(max) = between(rest, "more than this node takes over LoRa (", " KB)") {
            return format!("{kb} КБ > {max} (USB/BLE)");
        }
        if let Some(max) = between(rest, "over the ", " KB a node takes over LoRa by itself") {
            return format!("{kb} КБ > {max} (вручную)");
        }
        if let Some(name) = between(rest, "more than ", " hands on over LoRa") {
            return format!("{kb} КБ > лимит {name}");
        }
    }
    named
        .iter()
        .find_map(|(suffix, say)| why.strip_suffix(suffix).map(say))
        .unwrap_or_else(|| why.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_in_the_screens_words() {
        assert_eq!(reason("PKIO stopped answering"), "PKIO: нет ответа");
        assert_eq!(reason("no answer from PKIO"), "PKIO: нет ответа");
        assert_eq!(
            reason("nw.ommesh.ru not reachable, no neighbour hands updates on (fw.serve)"),
            "ommesh недоступен: Нет соседей OTA"
        );
        assert_eq!(
            reason("the update is 180 KB, more than this node takes over LoRa (160 KB): install it over USB or BLE"),
            "180 КБ > 160 (USB/BLE)"
        );
        assert_eq!(
            reason("the update is 180 KB, over the 160 KB a node takes over LoRa by itself: fw update takes it"),
            "180 КБ > 160 (вручную)"
        );
        assert_eq!(
            reason("the update is 180 KB, more than PKIO hands on over LoRa: install it with fw ble or USB"),
            "180 КБ > лимит PKIO"
        );
        assert_eq!(reason("OTA: commit failed: Flash"), "OTA: commit failed: Flash");
    }
}
