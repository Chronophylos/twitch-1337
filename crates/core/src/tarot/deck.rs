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
        name: "The Fool",
        arcana: Arcana::Major,
        suit: None,
        upright: "A fresh leap — trust the open road more than the map.",
        reversed: "Hesitation or recklessness; look before the leap, or take it already.",
    },
    Card {
        id: "major_01_magician",
        name: "The Magician",
        arcana: Arcana::Major,
        suit: None,
        upright: "The tools are on the table — will meets skill, and the trick is showing up.",
        reversed: "Scattered focus or sleight-of-hand with yourself; reclaim the wand.",
    },
    Card {
        id: "major_02_high_priestess",
        name: "The High Priestess",
        arcana: Arcana::Major,
        suit: None,
        upright: "Sit with the veil — what you already sense is the first answer.",
        reversed: "Noise over intuition; the scroll stays closed until you listen inward.",
    },
    Card {
        id: "major_03_empress",
        name: "The Empress",
        arcana: Arcana::Major,
        suit: None,
        upright: "Abundance in bloom — nurture what grows and let beauty do its work.",
        reversed: "Neglect or smothering; the garden needs tending, not control.",
    },
    Card {
        id: "major_04_emperor",
        name: "The Emperor",
        arcana: Arcana::Major,
        suit: None,
        upright: "Structure holds the throne — set boundaries and lead with steady authority.",
        reversed: "Rigidity or abdication; power without warmth becomes a cage.",
    },
    Card {
        id: "major_05_hierophant",
        name: "The Hierophant",
        arcana: Arcana::Major,
        suit: None,
        upright: "Tradition as a bridge — learn the rite, then speak it in your own voice.",
        reversed: "Empty ritual or rebellion for its own sake; question the rulebook gently.",
    },
    Card {
        id: "major_06_lovers",
        name: "The Lovers",
        arcana: Arcana::Major,
        suit: None,
        upright: "A real choice with heart in it — alignment matters more than perfection.",
        reversed: "Disharmony or avoidance; name what you want before the garden splits.",
    },
    Card {
        id: "major_07_chariot",
        name: "The Chariot",
        arcana: Arcana::Major,
        suit: None,
        upright: "Hold the reins — opposing forces can pull you forward if you steer.",
        reversed: "Stalled momentum or force without direction; loosen the grip and recenter.",
    },
    Card {
        id: "major_08_strength",
        name: "Strength",
        arcana: Arcana::Major,
        suit: None,
        upright: "Gentle courage tames the lion — patience outlasts brute force.",
        reversed: "Self-doubt or bottled rage; kindness toward yourself unlocks the chain.",
    },
    Card {
        id: "major_09_hermit",
        name: "The Hermit",
        arcana: Arcana::Major,
        suit: None,
        upright: "The lantern is inward — solitude clarifies what the crowd cannot.",
        reversed: "Isolation or refusing counsel; step back, but do not disappear.",
    },
    Card {
        id: "major_10_wheel_of_fortune",
        name: "Wheel of Fortune",
        arcana: Arcana::Major,
        suit: None,
        upright: "The wheel turns — ride the upswing and remember nothing stays on top forever.",
        reversed: "Bad luck streak or resisting change; adapt before the spoke catches you.",
    },
    Card {
        id: "major_11_justice",
        name: "Justice",
        arcana: Arcana::Major,
        suit: None,
        upright: "The scales demand honesty — cause meets consequence in plain sight.",
        reversed: "Bias or evasion; what is unbalanced will tip unless you address it.",
    },
    Card {
        id: "major_12_hanged_man",
        name: "The Hanged Man",
        arcana: Arcana::Major,
        suit: None,
        upright: "Suspend the rush — a new angle appears when you stop forcing the outcome.",
        reversed: "Stagnation or martyrdom; surrender with purpose, not as punishment.",
    },
    Card {
        id: "major_13_death",
        name: "Death",
        arcana: Arcana::Major,
        suit: None,
        upright: "An ending clears the field — let the old form fall so something truer can rise.",
        reversed: "Clinging or fear of transition; the chapter ends whether you turn the page.",
    },
    Card {
        id: "major_14_temperance",
        name: "Temperance",
        arcana: Arcana::Major,
        suit: None,
        upright: "Blend with care — moderation is alchemy, not blandness.",
        reversed: "Excess or impatience; pour slower until the mix holds.",
    },
    Card {
        id: "major_15_devil",
        name: "The Devil",
        arcana: Arcana::Major,
        suit: None,
        upright: "Chains you chose — name the habit and see the lock was never fully closed.",
        reversed: "Breaking a bond or denial lifting; freedom starts when you admit the hook.",
    },
    Card {
        id: "major_16_tower",
        name: "The Tower",
        arcana: Arcana::Major,
        suit: None,
        upright: "Lightning on false foundations — sudden truth is brutal but honest.",
        reversed: "Disaster dodged or delayed collapse; rebuild on bedrock this time.",
    },
    Card {
        id: "major_17_star",
        name: "The Star",
        arcana: Arcana::Major,
        suit: None,
        upright: "Hope after the storm — pour faith back into the cup and look up.",
        reversed: "Discouragement or distant dreams; the water is still there if you kneel.",
    },
    Card {
        id: "major_18_moon",
        name: "The Moon",
        arcana: Arcana::Major,
        suit: None,
        upright: "Walk the uncertain path — not everything in the dark is an enemy.",
        reversed: "Illusions fading or anxiety surfacing; trust your feet more than the shadows.",
    },
    Card {
        id: "major_19_sun",
        name: "The Sun",
        arcana: Arcana::Major,
        suit: None,
        upright: "Clarity and warmth — success shines simple when you stop hiding.",
        reversed: "Clouded joy or delayed celebration; the child still laughs behind the veil.",
    },
    Card {
        id: "major_20_judgement",
        name: "Judgement",
        arcana: Arcana::Major,
        suit: None,
        upright: "The call to rise — reckon with the past and answer who you are becoming.",
        reversed: "Self-judgment or ignored summons; forgiveness is the trumpet you keep muffling.",
    },
    Card {
        id: "major_21_world",
        name: "The World",
        arcana: Arcana::Major,
        suit: None,
        upright: "Completion and integration — the circle closes and the dance begins again.",
        reversed: "Almost there or loose ends; finish the last stitch before you bow.",
    },
    // --- wands ---
    Card {
        id: "wands_01",
        name: "Ace of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "A spark lands — seize the inspiration before the kindling cools.",
        reversed: "False starts or dampened fire; wait for a flame that actually catches.",
    },
    Card {
        id: "wands_02",
        name: "Two of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "The world in your hand — plan the voyage, then step off the balcony.",
        reversed: "Fear of the unknown or plans without action; pick a horizon.",
    },
    Card {
        id: "wands_03",
        name: "Three of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Ships on the horizon — expansion is underway; watch for their return.",
        reversed: "Delays or narrow vision; the fleet may need a nudge from shore.",
    },
    Card {
        id: "wands_04",
        name: "Four of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Homecoming and celebration — stable ground deserves a garland.",
        reversed: "Domestic friction or postponed joy; mend the threshold before the party.",
    },
    Card {
        id: "wands_05",
        name: "Five of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Friendly sparring — competition sharpens you if nobody takes it personal.",
        reversed: "Conflict avoidance or petty battles; agree on the rules or walk away.",
    },
    Card {
        id: "wands_06",
        name: "Six of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Victory lap earned — recognition arrives; accept it without shrinking.",
        reversed: "Ego bruise or hollow win; fame fades if the work stops.",
    },
    Card {
        id: "wands_07",
        name: "Seven of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Hold the high ground — defend your position with conviction, not panic.",
        reversed: "Overwhelmed or giving ground too fast; choose which hill matters.",
    },
    Card {
        id: "wands_08",
        name: "Eight of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Messages in flight — momentum accelerates; stay ready to catch what arrives.",
        reversed: "Delays or crossed wires; aim before you loose another arrow.",
    },
    Card {
        id: "wands_09",
        name: "Nine of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Battle-weary but standing — one more stand; the finish line is closer than it feels.",
        reversed: "Paranoia or burnout; lower the bandage and ask for backup.",
    },
    Card {
        id: "wands_10",
        name: "Ten of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Burden of success — you carried it this far; delegate before your back gives.",
        reversed: "Dropping obligations or refusing help; lighten the load on purpose.",
    },
    Card {
        id: "wands_page",
        name: "Page of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Curious messenger — a new adventure knocks; answer with enthusiasm.",
        reversed: "Restless or flaky energy; enthusiasm without follow-through fizzles.",
    },
    Card {
        id: "wands_knight",
        name: "Knight of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Charge ahead — passion drives the horse; steer before you gallop off a cliff.",
        reversed: "Impulsive or scattered; passion without direction burns the field.",
    },
    Card {
        id: "wands_queen",
        name: "Queen of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Magnetic confidence — lead with warmth and the room follows your flame.",
        reversed: "Jealousy or performative boldness; let authenticity replace the spotlight.",
    },
    Card {
        id: "wands_king",
        name: "King of Wands",
        arcana: Arcana::Minor,
        suit: Some(Suit::Wands),
        upright: "Visionary command — bold leadership with a long view and a steady torch.",
        reversed: "Tyranny or empty bravado; authority without vision scorches allies.",
    },
    // --- cups ---
    Card {
        id: "cups_01",
        name: "Ace of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Overflow of feeling — love or compassion pours fresh; cup both hands.",
        reversed: "Emotional block or spilled blessings; open the heart carefully.",
    },
    Card {
        id: "cups_02",
        name: "Two of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Mutual regard — partnership flows when both cups meet halfway.",
        reversed: "Imbalance or broken truce; honesty mends what politeness papers over.",
    },
    Card {
        id: "cups_03",
        name: "Three of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Raise a glass together — friendship and community refill the well.",
        reversed: "Gossip or excess; celebration turns sour if someone is left out.",
    },
    Card {
        id: "cups_04",
        name: "Four of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Contemplative boredom — the offered cup waits; look up from the sulking.",
        reversed: "Renewed interest or restless apathy breaking; accept the surprise.",
    },
    Card {
        id: "cups_05",
        name: "Five of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Mourn the spill — grief is valid, but two cups still stand behind you.",
        reversed: "Moving on or dwelling too long; turn around when mourning becomes a rut.",
    },
    Card {
        id: "cups_06",
        name: "Six of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Sweet memory — nostalgia heals when you share the story, not trap inside it.",
        reversed: "Stuck in the past or innocence lost; honor memory without living there.",
    },
    Card {
        id: "cups_07",
        name: "Seven of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Many visions, one choice — fantasies glitter; discernment picks the real cup.",
        reversed: "Clarity emerging or illusion lifting; narrow the options before you drown.",
    },
    Card {
        id: "cups_08",
        name: "Eight of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Walk away from what no longer fills — the moonlit path asks for courage.",
        reversed: "Fear of leaving or returning too soon; know why you go and where.",
    },
    Card {
        id: "cups_09",
        name: "Nine of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Wish granted — satisfaction sits heavy and good; enjoy without guilt.",
        reversed: "Shallow pleasure or unmet craving; the feast lacks the one thing you wanted.",
    },
    Card {
        id: "cups_10",
        name: "Ten of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Emotional fulfillment — harmony at home; the rainbow is the point.",
        reversed: "Domestic fracture or picture-perfect mask; repair the feeling, not the photo.",
    },
    Card {
        id: "cups_page",
        name: "Page of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Gentle surprise — creative or romantic news arrives like a fish in the cup.",
        reversed: "Emotional immaturity or creative block; feel it fully, then express it.",
    },
    Card {
        id: "cups_knight",
        name: "Knight of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Romantic pursuit — follow the heart's invitation with grace, not fantasy alone.",
        reversed: "Moodiness or unrealistic ideal; charm without substance leaves an empty cup.",
    },
    Card {
        id: "cups_queen",
        name: "Queen of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Deep empathy — intuition reads the room; hold space without absorbing everything.",
        reversed: "Emotional overwhelm or manipulation; boundaries protect the compassionate heart.",
    },
    Card {
        id: "cups_king",
        name: "King of Cups",
        arcana: Arcana::Minor,
        suit: Some(Suit::Cups),
        upright: "Calm mastery of feeling — lead with emotional intelligence through rough seas.",
        reversed: "Suppressed emotion or cold control; the king drowns when he forbids the tide.",
    },
    // --- swords ---
    Card {
        id: "swords_01",
        name: "Ace of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Cutting clarity — truth breaks through; wield the blade with precision.",
        reversed: "Confusion or harsh words; sharpen thought before you swing.",
    },
    Card {
        id: "swords_02",
        name: "Two of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Stalemate of mind — the blindfold comes off when you choose a side.",
        reversed: "Indecision cracking or forced peace; face what the crossed swords guard.",
    },
    Card {
        id: "swords_03",
        name: "Three of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Heart pierced by truth — sorrow hurts because it mattered; let it rain.",
        reversed: "Healing begun or repressed grief; remove the swords slowly, not all at once.",
    },
    Card {
        id: "swords_04",
        name: "Four of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Sacred rest — retreat recharges the mind; silence is strategy.",
        reversed: "Restlessness or forced isolation; sleep before the next battle.",
    },
    Card {
        id: "swords_05",
        name: "Five of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Pyrrhic victory — winning the argument may cost the relationship.",
        reversed: "Making amends or walking away from a hollow win; pride is the real loser.",
    },
    Card {
        id: "swords_06",
        name: "Six of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Transition by water — leave troubled shores; healing is the destination.",
        reversed: "Stuck baggage or resisted change; the boat waits but will not wait forever.",
    },
    Card {
        id: "swords_07",
        name: "Seven of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Stealth or strategy — not every battle is fought in the open; know your ethics.",
        reversed: "Exposure or self-deception; the stolen swords weigh more than they save.",
    },
    Card {
        id: "swords_08",
        name: "Eight of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Self-imposed limits — the bonds are loose; freedom starts in the mind.",
        reversed: "Liberation or deeper entrapment; cut one thread and test the rest.",
    },
    Card {
        id: "swords_09",
        name: "Nine of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Anxiety at night — the mind invents monsters; dawn shrinks them.",
        reversed: "Recovery from worry or spiraling thoughts; speak the fear aloud.",
    },
    Card {
        id: "swords_10",
        name: "Ten of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Rock bottom of thought — the worst is behind you; sunrise is literal.",
        reversed: "Survival or refusing to end a dead situation; get up when the back is clear.",
    },
    Card {
        id: "swords_page",
        name: "Page of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Keen observer — ideas arrive fast; verify before you broadcast.",
        reversed: "Gossip or scattered intellect; curiosity needs discipline.",
    },
    Card {
        id: "swords_knight",
        name: "Knight of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Swift intellect — charge toward truth; remember allies in the rush.",
        reversed: "Reckless speech or burnout; speed without aim cuts the wrong target.",
    },
    Card {
        id: "swords_queen",
        name: "Queen of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Clear-eyed honesty — boundaries and wit protect an open heart.",
        reversed: "Cold judgment or bitterness; sharpness without compassion wounds allies.",
    },
    Card {
        id: "swords_king",
        name: "King of Swords",
        arcana: Arcana::Minor,
        suit: Some(Suit::Swords),
        upright: "Intellectual authority — fair judgment from the throne of reason.",
        reversed: "Tyranny of logic or manipulation; power without mercy becomes a blade.",
    },
    // --- pentacles ---
    Card {
        id: "pentacles_01",
        name: "Ace of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Seed of prosperity — a tangible opportunity roots if you plant it now.",
        reversed: "Missed chance or shaky foundation; ground the coin before spending spirit.",
    },
    Card {
        id: "pentacles_02",
        name: "Two of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Juggling resources — balance is a dance; flexibility beats rigid plans.",
        reversed: "Overwhelm or poor prioritization; drop a ball before they all fall.",
    },
    Card {
        id: "pentacles_03",
        name: "Three of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Craft recognized — collaboration builds something that lasts.",
        reversed: "Sloppy work or ignored expertise; respect the blueprint.",
    },
    Card {
        id: "pentacles_04",
        name: "Four of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Security and holding — save wisely, but clutching starves growth.",
        reversed: "Greed loosening or financial fear; share or invest before the grip numbs.",
    },
    Card {
        id: "pentacles_05",
        name: "Five of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Hard times outside — hardship is real; help may be closer than the stained glass.",
        reversed: "Recovery or isolation ending; accept aid and warm the hands.",
    },
    Card {
        id: "pentacles_06",
        name: "Six of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Give and receive — generosity flows both ways when scales stay honest.",
        reversed: "Strings attached or unequal exchange; charity without dignity helps no one.",
    },
    Card {
        id: "pentacles_07",
        name: "Seven of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Patient harvest — assess the crop; long work ripens on its own schedule.",
        reversed: "Impatience or misinvested effort; replant if the vine will not bear.",
    },
    Card {
        id: "pentacles_08",
        name: "Eight of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Mastery through repetition — show up at the bench; skill is carved daily.",
        reversed: "Perfectionism or cutting corners; craft needs pride, not obsession.",
    },
    Card {
        id: "pentacles_09",
        name: "Nine of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Self-made comfort — enjoy what discipline built; independence is the garden.",
        reversed: "Loneliness in success or financial worry; wealth without connection feels hollow.",
    },
    Card {
        id: "pentacles_10",
        name: "Ten of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Legacy and stability — family wealth spans generations when roots run deep.",
        reversed: "Family dispute or crumbling inheritance; tend the tree, not just the coins.",
    },
    Card {
        id: "pentacles_page",
        name: "Page of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Student of the material — study, apprenticeship, or good news about money.",
        reversed: "Procrastination or unrealistic goals; ground the dream in a lesson plan.",
    },
    Card {
        id: "pentacles_knight",
        name: "Knight of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Steady progress — slow hooves cover distance; reliability is the superpower.",
        reversed: "Stagnation or stubborn routine; persistence needs a destination.",
    },
    Card {
        id: "pentacles_queen",
        name: "Queen of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Practical nurture — comfort and competence keep the hearth and ledger aligned.",
        reversed: "Neglect of self or work-life skew; abundance includes your own cup.",
    },
    Card {
        id: "pentacles_king",
        name: "King of Pentacles",
        arcana: Arcana::Minor,
        suit: Some(Suit::Pentacles),
        upright: "Material mastery — build empires with patience and a generous hand.",
        reversed: "Materialism or rigidity; gold without wisdom buys a gilded cage.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn deck_has_78_unique_ids() {
        assert_eq!(DECK.len(), 78);
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
    fn weights_match_arcana() {
        let major = DECK
            .iter()
            .find(|c| matches!(c.arcana, Arcana::Major))
            .unwrap();
        let minor = DECK
            .iter()
            .find(|c| matches!(c.arcana, Arcana::Minor))
            .unwrap();
        assert_eq!(card_weight(major), MAJOR_WEIGHT);
        assert_eq!(card_weight(minor), MINOR_WEIGHT);
    }

    #[test]
    fn deck_has_22_majors_and_56_minors() {
        let majors = DECK
            .iter()
            .filter(|c| matches!(c.arcana, Arcana::Major))
            .count();
        let minors = DECK
            .iter()
            .filter(|c| matches!(c.arcana, Arcana::Minor))
            .count();
        assert_eq!(majors, 22);
        assert_eq!(minors, 56);
    }
}
