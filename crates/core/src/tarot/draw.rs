use rand::Rng;
use rand::RngExt as _;

use super::deck::{Card, DECK, card_weight};
use super::spreads::THREE_CARD_LABELS;

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

pub fn draw_three_card(rng: &mut impl Rng) -> ThreeCardDraw {
    let mut pool: Vec<&'static Card> = DECK.iter().collect();
    let mut positions = [DrawnCard {
        card: &DECK[0],
        orientation: Orientation::Upright,
        label: THREE_CARD_LABELS[0],
    }; 3];

    for (i, slot) in positions.iter_mut().enumerate() {
        let card = draw_weighted_card(rng, &mut pool);
        let orientation = if rng.random::<bool>() {
            Orientation::Upright
        } else {
            Orientation::Reversed
        };
        *slot = DrawnCard {
            card,
            orientation,
            label: THREE_CARD_LABELS[i],
        };
    }

    ThreeCardDraw { positions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::SmallRng;
    use std::collections::HashSet;

    #[test]
    fn draw_has_three_distinct_cards_and_labels() {
        let mut rng = SmallRng::seed_from_u64(42);
        let draw = draw_three_card(&mut rng);
        let ids: HashSet<_> = draw.positions.iter().map(|p| p.card.id).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(draw.positions[0].label, "Past");
        assert_eq!(draw.positions[1].label, "Present");
        assert_eq!(draw.positions[2].label, "Future");
    }

    #[test]
    fn meaning_tracks_orientation() {
        let card = &crate::tarot::DECK[0];
        let up = DrawnCard {
            card,
            orientation: Orientation::Upright,
            label: "Past",
        };
        let rev = DrawnCard {
            card,
            orientation: Orientation::Reversed,
            label: "Past",
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
            let draw = draw_three_card(&mut rng);
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
