#[derive(Debug, Clone, Copy)]
pub struct LookupAirport {
    pub icao: Option<&'static str>,
    pub iata: Option<&'static str>,
    pub name: &'static str,
    pub lat: f32,
    pub lon: f32,
}

include!(concat!(env!("OUT_DIR"), "/location_airport_db.rs"));

pub fn lookup_airports() -> &'static [LookupAirport] {
    LOOKUP_AIRPORTS
}

pub fn lookup_by_icao(code: &str) -> Option<&'static LookupAirport> {
    LOOKUP_AIRPORTS
        .iter()
        .find(|a| a.icao.is_some_and(|c| c.eq_ignore_ascii_case(code)))
}

pub fn lookup_by_iata(code: &str) -> Option<&'static LookupAirport> {
    LOOKUP_AIRPORTS
        .iter()
        .find(|a| a.iata.is_some_and(|c| c.eq_ignore_ascii_case(code)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eddf_and_fra_resolve() {
        let by_icao = lookup_by_icao("EDDF").expect("EDDF");
        assert!(by_icao.name.to_ascii_lowercase().contains("frankfurt"));
        assert!((by_icao.lat - 50.03).abs() < 0.1);
        let by_iata = lookup_by_iata("FRA").expect("FRA");
        assert_eq!(by_iata.icao, Some("EDDF"));
    }

    #[test]
    fn every_row_has_icao4_or_iata3() {
        for apt in lookup_airports() {
            let ok_icao = apt.icao.is_some_and(|c| c.len() == 4);
            let ok_iata = apt.iata.is_some_and(|c| c.len() == 3);
            assert!(ok_icao || ok_iata, "bad row name={}", apt.name);
        }
    }

    #[test]
    fn unusable_heliport_style_rows_absent() {
        // 00A is first line of source CSV today: heliport, empty IATA — must not bake.
        assert!(lookup_by_icao("00A").is_none());
    }
}
