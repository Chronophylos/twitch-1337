#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arcana {
    Major,
    Minor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suit {
    Wands,
    Cups,
    Swords,
    Pentacles,
}

#[derive(Debug, Clone, Copy)]
pub struct Card {
    pub id: &'static str,
    pub name: &'static str,
    pub arcana: Arcana,
    pub suit: Option<Suit>,
    pub upright: &'static str,
    pub reversed: &'static str,
}

pub const MAJOR_WEIGHT: f64 = 1.5;
pub const MINOR_WEIGHT: f64 = 1.0;

pub fn card_weight(card: &Card) -> f64 {
    match card.arcana {
        Arcana::Major => MAJOR_WEIGHT,
        Arcana::Minor => MINOR_WEIGHT,
    }
}

pub static DECK: &[Card] = &[
    // --- 22 majors ---
    Card {
        id: "major_00_fool",
        name: "Der Narr",
        arcana: Arcana::Major,
        suit: None,
        upright: "Ein frischer Sprung. Vertrau der offenen Straße mehr als der Karte.",
        reversed: "Zögern oder Leichtsinn. Erst gucken, dann springen. Oder endlich springen.",
    },
    Card {
        id: "major_01_magician",
        name: "Der Magier",
        arcana: Arcana::Major,
        suit: None,
        upright: "Die Werkzeuge liegen bereit. Wille trifft Können, der Trick ist aufzutauchen.",
        reversed: "Zerstreuter Fokus oder Selbstbetrug. Hol den Stab zurück.",
    },
    Card {
        id: "major_02_high_priestess",
        name: "Die Hohepriesterin",
        arcana: Arcana::Major,
        suit: None,
        upright: "Bleib beim Schleier. Was du schon spürst, ist die erste Antwort.",
        reversed: "Lärm statt Intuition. Die Schriftrolle bleibt zu, bis du nach innen hörst.",
    },
    Card {
        id: "major_03_empress",
        name: "Die Herrscherin",
        arcana: Arcana::Major,
        suit: None,
        upright: "Fülle in Blüte. Pfleg was wächst, lass Schönheit ihre Arbeit machen.",
        reversed: "Vernachlässigung oder Erstickung. Der Garten will Pflege, keine Kontrolle.",
    },
    Card {
        id: "major_04_emperor",
        name: "Der Herrscher",
        arcana: Arcana::Major,
        suit: None,
        upright: "Struktur hält den Thron. Setz Grenzen, führ mit ruhiger Autorität.",
        reversed: "Starrheit oder Abwälzen. Macht ohne Wärme wird zum Käfig.",
    },
    Card {
        id: "major_05_hierophant",
        name: "Der Hierophant",
        arcana: Arcana::Major,
        suit: None,
        upright: "Tradition als Brücke. Lern das Ritual, sprich es dann in deiner Stimme.",
        reversed: "Leeres Ritual oder Rebellion um ihrer selbst willen. Hinterfrag das Regelwerk sachte.",
    },
    Card {
        id: "major_06_lovers",
        name: "Die Liebenden",
        arcana: Arcana::Major,
        suit: None,
        upright: "Eine echte Wahl mit Herz. Ausrichtung zählt mehr als Perfektion.",
        reversed: "Disharmonie oder Ausweichen. Sag was du willst, bevor der Garten auseinanderläuft.",
    },
    Card {
        id: "major_07_chariot",
        name: "Der Wagen",
        arcana: Arcana::Major,
        suit: None,
        upright: "Halt die Zügel. Gegensätzliche Kräfte ziehen dich voran, wenn du steuerst.",
        reversed: "Hängen geblieben oder Kraft ohne Richtung. Lockern, neu zentrieren.",
    },
    Card {
        id: "major_08_strength",
        name: "Die Kraft",
        arcana: Arcana::Major,
        suit: None,
        upright: "Sanfter Mut zähmt den Löwen. Geduld schlägt rohe Gewalt.",
        reversed: "Selbstzweifel oder eingestaute Wut. Freundlichkeit zu dir selbst löst die Kette.",
    },
    Card {
        id: "major_09_hermit",
        name: "Der Eremit",
        arcana: Arcana::Major,
        suit: None,
        upright: "Die Laterne brennt innen. Einsamkeit klärt, was die Menge nicht kann.",
        reversed: "Isolation oder Rat verweigern. Tritt zurück, aber verschwinde nicht.",
    },
    Card {
        id: "major_10_wheel_of_fortune",
        name: "Das Rad des Schicksals",
        arcana: Arcana::Major,
        suit: None,
        upright: "Das Rad dreht sich. Reit die Welle nach oben, nichts bleibt ewig oben.",
        reversed: "Pechserie oder Widerstand gegen Wandel. Pass dich an, bevor ein Speichen dich trifft.",
    },
    Card {
        id: "major_11_justice",
        name: "Die Gerechtigkeit",
        arcana: Arcana::Major,
        suit: None,
        upright: "Die Waage will Ehrlichkeit. Ursache trifft Folge, offen sichtbar.",
        reversed: "Voreingenommenheit oder Ausweichen. Was schief liegt, kippt, wenn du es nicht angehst.",
    },
    Card {
        id: "major_12_hanged_man",
        name: "Der Gehängte",
        arcana: Arcana::Major,
        suit: None,
        upright: "Halt den Drang an. Ein neuer Blickwinkel kommt, wenn du das Ergebnis nicht erzwingst.",
        reversed: "Stillstand oder Märtyrertum. Loslassen mit Sinn, nicht als Strafe.",
    },
    Card {
        id: "major_13_death",
        name: "Der Tod",
        arcana: Arcana::Major,
        suit: None,
        upright: "Ein Ende räumt das Feld. Lass die alte Form fallen, damit etwas Wahres wachsen kann.",
        reversed: "Festhalten oder Angst vor Übergang. Das Kapitel endet, ob du blätterst oder nicht.",
    },
    Card {
        id: "major_14_temperance",
        name: "Die Mäßigkeit",
        arcana: Arcana::Major,
        suit: None,
        upright: "Mischen mit Bedacht. Maß ist Alchemie, nicht Langweile.",
        reversed: "Exzess oder Ungeduld. Langsamer gießen, bis die Mischung hält.",
    },
    Card {
        id: "major_15_devil",
        name: "Der Teufel",
        arcana: Arcana::Major,
        suit: None,
        upright: "Ketten, die du gewählt hast. Benenn die Gewohnheit, das Schloss war nie ganz zu.",
        reversed: "Band löst sich oder Verleugnung weicht. Freiheit beginnt, wenn du den Haken zugibst.",
    },
    Card {
        id: "major_16_tower",
        name: "Der Turm",
        arcana: Arcana::Major,
        suit: None,
        upright: "Blitz auf falschen Fundamenten. Plötzliche Wahrheit ist brutal, aber ehrlich.",
        reversed: "Katastrophe abgewendet oder verschobener Einsturz. Diesmal auf Fels neu bauen.",
    },
    Card {
        id: "major_17_star",
        name: "Der Stern",
        arcana: Arcana::Major,
        suit: None,
        upright: "Hoffnung nach dem Sturm. Gieß Glauben zurück in den Becher, schau nach oben.",
        reversed: "Mutlosigkeit oder ferne Träume. Das Wasser ist noch da, wenn du dich hinbeugst.",
    },
    Card {
        id: "major_18_moon",
        name: "Der Mond",
        arcana: Arcana::Major,
        suit: None,
        upright: "Geh den unsicheren Pfad. Nicht alles in der Dunkelheit ist Feind.",
        reversed: "Illusionen weichen oder Angst steigt auf. Vertrau deinen Füßen mehr als den Schatten.",
    },
    Card {
        id: "major_19_sun",
        name: "Die Sonne",
        arcana: Arcana::Major,
        suit: None,
        upright: "Klarheit und Wärme. Erfolg leuchtet simpel, wenn du aufhörst dich zu verstecken.",
        reversed: "Getrübte Freude oder verspätetes Feiern. Das Kind lacht noch hinter dem Schleier.",
    },
    Card {
        id: "major_20_judgement",
        name: "Das Gericht",
        arcana: Arcana::Major,
        suit: None,
        upright: "Der Ruf aufzustehen. Rechne mit der Vergangenheit, antworte wer du wirst.",
        reversed: "Selbstverurteilung oder verpasster Ruf. Vergebung ist die Trompete, die du dämpfst.",
    },
    Card {
        id: "major_21_world",
        name: "Die Welt",
        arcana: Arcana::Major,
        suit: None,
        upright: "Vollendung und Integration. Der Kreis schließt sich, der Tanz beginnt von vorn.",
        reversed: "Fast da oder lose Fäden. Den letzten Stich setzen, dann verbeugen.",
    },
    // --- wands ---
    Card {
        id: "wands_01",
        name: "Ass der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Ein Funke landet. Pack die Inspiration, bevor das Zunder abkühlt.",
        reversed: "Falscher Start oder gedämpftes Feuer. Warte auf eine Flamme, die wirklich fängt.",
    },
    Card {
        id: "wands_02",
        name: "Zwei der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Die Welt in der Hand. Plan die Reise, dann tritt vom Balkon.",
        reversed: "Angst vor dem Unbekannten oder Pläne ohne Tat. Wähl einen Horizont.",
    },
    Card {
        id: "wands_03",
        name: "Drei der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Schiffe am Horizont. Expansion läuft, acht auf ihre Rückkehr.",
        reversed: "Verzögerung oder enger Blick. Die Flotte braucht vielleicht einen Stupser vom Ufer.",
    },
    Card {
        id: "wands_04",
        name: "Vier der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Heimkehr und Fest. Stabiler Boden verdient einen Kranz.",
        reversed: "Häusliche Reibung oder aufgeschobene Freude. Reparier die Schwelle vor der Party.",
    },
    Card {
        id: "wands_05",
        name: "Fünf der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Freundschaftliches Sparring. Wettbewerb schärft dich, wenn's niemand persönlich nimmt.",
        reversed: "Konfliktvermeidung oder kleinkarierte Kämpfe. Einigt euch auf Regeln oder geht.",
    },
    Card {
        id: "wands_06",
        name: "Sechs der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Verdiente Ehrenrunde. Anerkennung kommt, nimm sie an ohne dich zu verkleinern.",
        reversed: "Verletzter Stolz oder hohler Sieg. Ruhm verblasst, wenn die Arbeit aufhört.",
    },
    Card {
        id: "wands_07",
        name: "Sieben der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Halt die hohe Position. Verteidige mit Überzeugung, nicht Panik.",
        reversed: "Überfordert oder zu schnell nachgeben. Wähl den Hügel, der zählt.",
    },
    Card {
        id: "wands_08",
        name: "Acht der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Botschaften im Flug. Schwung nimmt zu, sei bereit zu fangen was ankommt.",
        reversed: "Verzögerung oder missverstandene Signale. Zielen, bevor du den nächsten Pfeil schickst.",
    },
    Card {
        id: "wands_09",
        name: "Neun der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Kampfmüde, aber stehend. Noch ein Stand, die Ziellinie ist näher als es sich anfühlt.",
        reversed: "Paranoia oder Burnout. Verband lockern, um Verstärkung bitten.",
    },
    Card {
        id: "wands_10",
        name: "Zehn der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Last des Erfolgs. Du hast es bis hier getragen, delegier bevor der Rücken nachgibt.",
        reversed: "Pflichten abwerfen oder Hilfe ablehnen. Die Last bewusst leichter machen.",
    },
    Card {
        id: "wands_page",
        name: "Page der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Neugieriger Bote. Ein neues Abenteuer klopft, antwort mit Begeisterung.",
        reversed: "Rastlose oder wackelige Energie. Enthusiasmus ohne Durchziehen verpufft.",
    },
    Card {
        id: "wands_knight",
        name: "Ritter der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Voran galoppieren. Leidenschaft treibt das Pferd, steuer bevor du von der Klippe fliegst.",
        reversed: "Impulsiv oder zerstreut. Leidenschaft ohne Richtung verbrennt das Feld.",
    },
    Card {
        id: "wands_queen",
        name: "Königin der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Magnetisches Selbstvertrauen. Führ mit Wärme, der Raum folgt deiner Flamme.",
        reversed: "Eifersucht oder gespielte Tapferkeit. Authentizität statt Scheinwerfer.",
    },
    Card {
        id: "wands_king",
        name: "König der Stäbe",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Visionäres Kommando. Mutige Führung mit Weitblick und stabiler Fackel.",
        reversed: "Tyrannei oder leeres Getöse. Autorität ohne Vision versengt Verbündete.",
    },
    // --- cups ---
    Card {
        id: "cups_01",
        name: "Ass der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Gefühle quellen über. Liebe oder Mitgefühl fließt frisch, fang mit beiden Händen.",
        reversed: "Gefühlsblockade oder verschüttete Segen. Herz vorsichtig öffnen.",
    },
    Card {
        id: "cups_02",
        name: "Zwei der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Gegenseitige Wertschätzung. Partnerschaft fließt, wenn beide Becher sich treffen.",
        reversed: "Ungleichgewicht oder gebrochener Frieden. Ehrlichkeit heilt, was Höflichkeit überklebt.",
    },
    Card {
        id: "cups_03",
        name: "Drei der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Glas heben zusammen. Freundschaft und Community füllen den Brunnen.",
        reversed: "Tratsch oder Exzess. Feier wird sauer, wenn jemand fehlt.",
    },
    Card {
        id: "cups_04",
        name: "Vier der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Nachdenkliche Langeweile. Der angebotene Becher wartet, heb den Blick vom Grübeln.",
        reversed: "Erneutes Interesse oder rastlose Apathie bricht. Nimm die Überraschung an.",
    },
    Card {
        id: "cups_05",
        name: "Fünf der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Trauer um den Verlust. Trauer ist berechtigt, aber zwei Becher stehen noch hinter dir.",
        reversed: "Weitergehen oder zu lange verweilen. Dreh dich um, wenn Trauern zur Furche wird.",
    },
    Card {
        id: "cups_06",
        name: "Sechs der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Süße Erinnerung. Nostalgie heilt, wenn du die Geschichte teilst, nicht darin feststeckst.",
        reversed: "In der Vergangenheit hängen oder verlorene Unschuld. Ehre die Erinnerung, leb nicht darin.",
    },
    Card {
        id: "cups_07",
        name: "Sieben der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Viele Visionen, eine Wahl. Fantasien glitzern, Unterscheidungskraft wählt den echten Becher.",
        reversed: "Klarheit taucht auf oder Illusion weicht. Optionen eingrenzen, bevor du ertrinkst.",
    },
    Card {
        id: "cups_08",
        name: "Acht der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Geh weg von dem, was nicht mehr füllt. Der mondbeschienene Pfad will Mut.",
        reversed: "Angst zu gehen oder zu früh zurück. Wisse warum du gehst und wohin.",
    },
    Card {
        id: "cups_09",
        name: "Neun der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Wunsch erfüllt. Zufriedenheit sitzt schwer und gut, genieß ohne Schuld.",
        reversed: "Oberflächliches Vergnügen oder unstillbares Verlangen. Das Fest hat nicht das eine Ding, das du wolltest.",
    },
    Card {
        id: "cups_10",
        name: "Zehn der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Emotionale Erfüllung. Harmonie zu Hause, der Regenbogen ist der Punkt.",
        reversed: "Häuslicher Bruch oder makellose Fassade. Reparier das Gefühl, nicht nur das Foto.",
    },
    Card {
        id: "cups_page",
        name: "Page der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Sanfte Überraschung. Kreative oder romantische Nachricht wie ein Fisch im Becher.",
        reversed: "Gefühlsreife fehlt oder kreative Blockade. Ganz fühlen, dann ausdrücken.",
    },
    Card {
        id: "cups_knight",
        name: "Ritter der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Romantische Verfolgung. Folg der Einladung des Herzens mit Anstand, nicht nur Fantasie.",
        reversed: "Launen oder unrealistisches Ideal. Charme ohne Substanz lässt leeren Becher.",
    },
    Card {
        id: "cups_queen",
        name: "Königin der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Tiefe Empathie. Intuition liest den Raum, halte Raum ohne alles aufzusaugen.",
        reversed: "Gefühlsüberflutung oder Manipulation. Grenzen schützen das mitfühlende Herz.",
    },
    Card {
        id: "cups_king",
        name: "König der Kelche",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Ruhige Meisterschaft der Gefühle. Führ mit emotionalem Verstand durch raue See.",
        reversed: "Unterdrückte Gefühle oder kalte Kontrolle. Der König ertrinkt, wenn er die Flut verbietet.",
    },
    // --- swords ---
    Card {
        id: "swords_01",
        name: "Ass der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Schneidende Klarheit. Wahrheit bricht durch, führ die Klinge präzise.",
        reversed: "Verwirrung oder harte Worte. Denk schärfen, bevor du ausholst.",
    },
    Card {
        id: "swords_02",
        name: "Zwei der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Pattsituation im Kopf. Die Augenbinde fällt, wenn du eine Seite wählst.",
        reversed: "Unentschlossenheit bricht oder erzwungener Frieden. Schau was die gekreuzten Schwerter bewachen.",
    },
    Card {
        id: "swords_03",
        name: "Drei der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Herz von Wahrheit durchbohrt. Kummer tut weh, weil es zählte, lass es regnen.",
        reversed: "Heilung beginnt oder verdrängter Schmerz. Schwerter langsam ziehen, nicht alle auf einmal.",
    },
    Card {
        id: "swords_04",
        name: "Vier der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Heilige Ruhe. Rückzug lädt den Kopf auf, Stille ist Strategie.",
        reversed: "Ruhelosigkeit oder erzwungene Isolation. Schlaf vor der nächsten Schlacht.",
    },
    Card {
        id: "swords_05",
        name: "Fünf der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Pyrrhussieg. Das Argument gewinnen kann die Beziehung kosten.",
        reversed: "Wiedergutmachung oder Weggehen vom hohlen Sieg. Stolz ist der eigentliche Verlierer.",
    },
    Card {
        id: "swords_06",
        name: "Sechs der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Übergang übers Wasser. Verlass die trüben Ufer, Heilung ist das Ziel.",
        reversed: "Mitgepackte Last oder verweigerter Wandel. Das Boot wartet, aber nicht ewig.",
    },
    Card {
        id: "swords_07",
        name: "Sieben der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Heimlichkeit oder Strategie. Nicht jede Schlacht ist offen, kenn deine Ethik.",
        reversed: "Entlarvt oder Selbsttäuschung. Gestohlene Schwerter wiegen mehr als sie sparen.",
    },
    Card {
        id: "swords_08",
        name: "Acht der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Selbst gesetzte Grenzen. Die Fesseln sind locker, Freiheit beginnt im Kopf.",
        reversed: "Befreiung oder tiefere Falle. Einen Faden durchtrennen, den Rest testen.",
    },
    Card {
        id: "swords_09",
        name: "Neun der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Nachtangst. Der Kopf erfindet Monster, die Morgendämmerung schrumpft sie.",
        reversed: "Erholung von Sorge oder Gedankenspirale. Die Angst laut aussprechen.",
    },
    Card {
        id: "swords_10",
        name: "Zehn der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Tiefpunkt des Denkens. Das Schlimmste liegt hinter dir, der Sonnenaufgang ist wörtlich.",
        reversed: "Überlebt oder tote Situation nicht beenden. Aufstehen, wenn der Rücken frei ist.",
    },
    Card {
        id: "swords_page",
        name: "Page der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Scharfer Beobachter. Ideen kommen schnell, prüf bevor du sendest.",
        reversed: "Klatsch oder zerstreutes Denken. Neugier braucht Disziplin.",
    },
    Card {
        id: "swords_knight",
        name: "Ritter der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Schneller Verstand. Sturm auf die Wahrheit, denk an Verbündete im Rausch.",
        reversed: "Rücksichtsloses Reden oder Burnout. Tempo ohne Ziel trifft das Falsche.",
    },
    Card {
        id: "swords_queen",
        name: "Königin der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Klarer Blick, ehrliche Grenzen. Schärfe und Witz schützen ein offenes Herz.",
        reversed: "Kaltes Urteil oder Verbitterung. Schärfe ohne Mitgefühl verletzt Freunde.",
    },
    Card {
        id: "swords_king",
        name: "König der Schwerter",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Geistige Autorität. Faires Urteil vom Thron der Vernunft.",
        reversed: "Tyrannei der Logik oder Manipulation. Macht ohne Gnade wird zur Klinge.",
    },
    // --- pentacles ---
    Card {
        id: "pentacles_01",
        name: "Ass der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Samen von Wohlstand. Eine greifbare Chance wurzelt, wenn du sie jetzt pflanzt.",
        reversed: "Verpasste Chance oder wackliger Grund. Die Münze erden, bevor du den Geist ausgibst.",
    },
    Card {
        id: "pentacles_02",
        name: "Zwei der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Jonglieren mit Mitteln. Balance ist ein Tanz, Flexibilität schlägt starre Pläne.",
        reversed: "Überlastung oder falsche Prioritäten. Lass einen Ball fallen, bevor alle fallen.",
    },
    Card {
        id: "pentacles_03",
        name: "Drei der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Handwerk anerkannt. Zusammenarbeit baut etwas, das bleibt.",
        reversed: "Schlampige Arbeit oder ignorierte Expertise. Respektiere den Plan.",
    },
    Card {
        id: "pentacles_04",
        name: "Vier der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Sicherheit und Halten. Spare klug, aber zu fest umklammern erstickt Wachstum.",
        reversed: "Gier lockert oder finanzielle Angst. Teilen oder investieren, bevor die Finger taub werden.",
    },
    Card {
        id: "pentacles_05",
        name: "Fünf der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Harte Zeiten draußen. Not ist real, Hilfe kann näher sein als das Kirchenfenster.",
        reversed: "Erholung oder Isolation endet. Hilfe annehmen, die Hände wärmen.",
    },
    Card {
        id: "pentacles_06",
        name: "Sechs der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Geben und Nehmen. Großzügigkeit fließt beide Wege, wenn die Waage ehrlich bleibt.",
        reversed: "Fäden dran oder ungleicher Tausch. Almosen ohne Würde hilft niemandem.",
    },
    Card {
        id: "pentacles_07",
        name: "Sieben der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Geduldige Ernte. Schau die Frucht an, langsame Arbeit reift im eigenen Tempo.",
        reversed: "Ungeduld oder vergeudeter Einsatz. Umpflanzen, wenn der Weinstock nicht trägt.",
    },
    Card {
        id: "pentacles_08",
        name: "Acht der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Meisterschaft durch Wiederholung. An die Werkbank, Können wird täglich gemeißelt.",
        reversed: "Perfektionismus oder Abkürzungen. Handwerk braucht Stolz, nicht Besessenheit.",
    },
    Card {
        id: "pentacles_09",
        name: "Neun der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Selbst erarbeiteter Komfort. Genieß was Disziplin baute, Unabhängigkeit ist der Garten.",
        reversed: "Einsamkeit im Erfolg oder Geldsorgen. Reichtum ohne Verbindung fühlt sich leer an.",
    },
    Card {
        id: "pentacles_10",
        name: "Zehn der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Erbe und Stabilität. Familienwohl reicht über Generationen, wenn die Wurzeln tief sind.",
        reversed: "Familienstreit oder bröckelndes Erbe. Pfleg den Baum, nicht nur die Münzen.",
    },
    Card {
        id: "pentacles_page",
        name: "Page der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Schüler des Materiellen. Lernen, Lehre oder gute Nachricht über Geld.",
        reversed: "Aufschieben oder unrealistische Ziele. Träum in einen Lernplan erden.",
    },
    Card {
        id: "pentacles_knight",
        name: "Ritter der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Stetiger Fortschritt. Langsame Hufe legen Strecke zurück, Zuverlässigkeit ist die Superkraft.",
        reversed: "Stillstand oder sturer Trott. Ausdauer braucht ein Ziel.",
    },
    Card {
        id: "pentacles_queen",
        name: "Königin der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Praktische Fürsorge. Behaglichkeit und Kompetenz halten Herd und Kassenbuch im Gleichklang.",
        reversed: "Selbstvernachlässigung oder Schieflage zwischen Arbeit und Leben. Fülle schließt deinen eigenen Becher ein.",
    },
    Card {
        id: "pentacles_king",
        name: "König der Münzen",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Materielle Meisterschaft. Baue Imperien mit Geduld und großzügiger Hand.",
        reversed: "Materialismus oder Starrheit. Gold ohne Weisheit kauft einen vergoldeten Käfig.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn deck_has_22_majors_56_minors_and_unique_ids() {
        let majors = DECK
            .iter()
            .filter(|c| matches!(c.arcana, Arcana::Major))
            .count();
        let minors = DECK
            .iter()
            .filter(|c| matches!(c.arcana, Arcana::Minor))
            .count();
        assert_eq!((majors, minors), (22, 56));
        let ids: HashSet<_> = DECK.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), 78);
    }

    #[test]
    fn every_card_has_nonempty_meanings_and_name() {
        for c in DECK {
            assert!(!c.id.is_empty(), "empty id");
            assert!(!c.name.is_empty(), "{}", c.id);
            assert!(!c.upright.is_empty(), "{}", c.id);
            assert!(!c.reversed.is_empty(), "{}", c.id);
            match c.arcana {
                Arcana::Major => assert!(c.suit.is_none(), "{}", c.id),
                Arcana::Minor => assert!(c.suit.is_some(), "{}", c.id),
            }
        }
    }

    #[test]
    fn meanings_have_no_em_or_en_dashes() {
        for c in DECK {
            for text in [c.upright, c.reversed] {
                assert!(
                    !text.contains('\u{2014}') && !text.contains('\u{2013}'),
                    "{} has forbidden dash: {text}",
                    c.id
                );
            }
        }
    }
}
