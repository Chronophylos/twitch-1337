use rand::Rng;
use rand::RngExt as _;

use super::deck::{Card, DECK, card_weight};
use super::spreads::SINGLE_CARD_LABEL;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Upright,
    Reversed,
}

#[derive(Debug, Clone, Copy)]
pub struct DrawnCard {
    pub card: &'static Card,
    pub orientation: Orientation,
    pub label: &'static str,
}

impl DrawnCard {
    pub fn meaning(&self) -> &'static str {
        match self.orientation {
            Orientation::Upright => self.card.upright,
            Orientation::Reversed => self.card.reversed,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThreeCardDraw {
    pub positions: [DrawnCard; 3],
}

#[derive(Debug, Clone)]
pub struct SingleCardDraw {
    pub card: DrawnCard,
}

fn draw_weighted_card(rng: &mut impl Rng, pool: &mut Vec<&'static Card>) -> &'static Card {
    let weights: Vec<f64> = pool.iter().map(|c| card_weight(c)).collect();
    let total: f64 = weights.iter().sum();
    let mut roll = rng.random::<f64>() * total;
    let mut pick = weights.len() - 1;
    for (i, w) in weights.iter().enumerate() {
        if roll < *w {
            pick = i;
            break;
        }
        roll -= *w;
    }
    pool.swap_remove(pick)
}

fn flip_orientation(rng: &mut impl Rng) -> Orientation {
    if rng.random::<bool>() {
        Orientation::Upright
    } else {
        Orientation::Reversed
    }
}

pub fn draw_three_card(rng: &mut impl Rng, labels: [&'static str; 3]) -> ThreeCardDraw {
    let mut pool: Vec<&'static Card> = DECK.iter().collect();
    let mut positions = [DrawnCard {
        card: &DECK[0],
        orientation: Orientation::Upright,
        label: labels[0],
    }; 3];

    for (i, slot) in positions.iter_mut().enumerate() {
        let card = draw_weighted_card(rng, &mut pool);
        *slot = DrawnCard {
            card,
            orientation: flip_orientation(rng),
            label: labels[i],
        };
    }

    ThreeCardDraw { positions }
}

pub fn draw_single_card(rng: &mut impl Rng) -> SingleCardDraw {
    let mut pool: Vec<&'static Card> = DECK.iter().collect();
    let card = draw_weighted_card(rng, &mut pool);
    SingleCardDraw {
        card: DrawnCard {
            card,
            orientation: flip_orientation(rng),
            label: SINGLE_CARD_LABEL,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tarot::spreads::{FUTURE_LABELS, GUIDANCE_LABELS};
    use rand::SeedableRng;
    use rand::rngs::SmallRng;
    use std::collections::HashSet;

    #[test]
    fn future_draw_labels() {
        let mut rng = SmallRng::seed_from_u64(42);
        let draw = draw_three_card(&mut rng, FUTURE_LABELS);
        let ids: HashSet<_> = draw.positions.iter().map(|p| p.card.id).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(draw.positions[0].label, "Vergangenheit");
        assert_eq!(draw.positions[1].label, "Gegenwart");
        assert_eq!(draw.positions[2].label, "Zukunft");
    }

    #[test]
    fn guidance_draw_labels() {
        let mut rng = SmallRng::seed_from_u64(42);
        let draw = draw_three_card(&mut rng, GUIDANCE_LABELS);
        assert_eq!(draw.positions[0].label, "Ursache");
        assert_eq!(draw.positions[1].label, "Hindernis");
        assert_eq!(draw.positions[2].label, "Rat");
    }

    #[test]
    fn single_draw_has_day_label() {
        let mut rng = SmallRng::seed_from_u64(42);
        let draw = draw_single_card(&mut rng);
        assert_eq!(draw.card.label, "Day");
        assert!(!draw.card.meaning().is_empty());
    }

    #[test]
    fn meaning_tracks_orientation() {
        let card = &crate::tarot::DECK[0];
        let up = DrawnCard {
            card,
            orientation: Orientation::Upright,
            label: "Vergangenheit",
        };
        let rev = DrawnCard {
            card,
            orientation: Orientation::Reversed,
            label: "Vergangenheit",
        };
        assert_eq!(up.meaning(), card.upright);
        assert_eq!(rev.meaning(), card.reversed);
    }

    #[test]
    fn majors_appear_more_often_than_uniform_share() {
        let mut rng = SmallRng::seed_from_u64(7);
        const N: usize = 3000;
        let mut major_first = 0usize;
        for _ in 0..N {
            let draw = draw_three_card(&mut rng, FUTURE_LABELS);
            if matches!(draw.positions[0].card.arcana, crate::tarot::Arcana::Major) {
                major_first += 1;
            }
        }
        let rate = major_first as f64 / N as f64;
        assert!(
            rate > 0.32,
            "expected soft major bias on first card, got rate={rate}"
        );
    }
}
