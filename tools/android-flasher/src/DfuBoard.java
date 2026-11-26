package ru.ommesh.otg;

import java.util.Locale;

enum DfuBoard {
    T096("heltec-t096", "Heltec T096"),
    T114("heltec-t114", "Heltec T114"),
    PRO_MICRO("promicro", "Pro Micro (FakeTec)"),
    T_ECHO("t-echo", "LilyGO T-Echo");

    final String releaseName;
    final String title;

    DfuBoard(String releaseName, String title) {
        this.releaseName = releaseName;
        this.title = title;
    }

    static DfuBoard fromAdvertisement(String name) {
        switch (normalize(name)) {
            case "t096dfu": case "heltect096dfu":
            case "htn5262gota": case "htnrf5262gota":
                return T096;
            case "t114dfu": case "heltect114dfu":
            case "htn5262ota": case "htnrf5262ota":
                return T114;
            case "promdfu": case "promicrodfu": case "promicroota":
            case "faketecdfu": case "faketecota":
                return PRO_MICRO;
            case "lgtedfu": case "techodfu": case "techoota": case "lilygotechodfu":
                return T_ECHO;
            default:
                return null;
        }
    }

    static DfuBoard fromFirmwareName(String filename) {
        String name = normalize(filename);
        for (DfuBoard board : values()) {
            if (name.startsWith(normalize(board.releaseName))) return board;
        }
        if (name.startsWith("t096")) return T096;
        if (name.startsWith("t114")) return T114;
        if (name.startsWith("faketec") || name.startsWith("nrf52840promicro")) return PRO_MICRO;
        if (name.startsWith("lilygotecho")) return T_ECHO;
        return null;
    }

    static DfuBoard fromUsbIdentity(String model,String boardId) {
        String identity=normalize(model+" "+boardId);
        if(identity.contains("t096")||identity.contains("n5262g")||identity.contains("nrf5262g"))return T096;
        if(identity.contains("t114"))return T114;
        if(identity.contains("promicro")||identity.contains("faketec"))return PRO_MICRO;
        if(identity.contains("techo")&&!identity.contains("techolite")&&!identity.contains("techoplus")
            &&!model.contains("+")&&!identity.contains("ttgoeink"))return T_ECHO;
        // HT-n5262 is used by more than one Heltec model; USB alone cannot distinguish them.
        return null;
    }

    private static String normalize(String name) {
        return name == null ? "" : name.toLowerCase(Locale.ROOT).replaceAll("[^a-z0-9]", "");
    }
}
