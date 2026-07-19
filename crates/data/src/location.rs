//! Packed location tables (lookup airports + German PLZ).
//!
//! Build emits flat little-endian blobs (`include_bytes!`) so PIE reloc tables
//! stay tiny — unlike `&str`-heavy generated Rust structs.

use std::str;
use std::sync::OnceLock;

/// One lookup airport decoded from the packed blob.
///
/// String fields borrow into the static blob (runtime slices), not into a
/// reloc-heavy static struct table.
#[derive(Debug, Clone, Copy)]
pub struct LookupAirport {
    pub icao: Option<&'static str>,
    pub iata: Option<&'static str>,
    pub name: &'static str,
    pub lat: f32,
    pub lon: f32,
}

/// One PLZ coordinate row decoded from the packed blob.
#[derive(Debug, Clone, Copy)]
pub struct PlzEntry {
    pub plz: &'static str,
    pub lat: f32,
    pub lon: f32,
}

const LOOKUP_MAGIC: &[u8; 4] = b"LKA1";
const LOOKUP_HEADER_LEN: usize = 12;
const LOOKUP_ROW_SIZE: usize = 24;
const LOOKUP_FLAG_ICAO: u8 = 1;
const LOOKUP_FLAG_IATA: u8 = 2;

const PLZ_MAGIC: &[u8; 4] = b"PLZ1";
const PLZ_HEADER_LEN: usize = 8;
const PLZ_ROW_SIZE: usize = 16;

static LOOKUP_AIRPORT_BLOB: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/location_airports.bin"));
static PLZ_BLOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plz.bin"));

fn read_u32_le(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(bytes[off..off + 4].try_into().expect("u32 slice"))
}

fn read_f32_le(bytes: &[u8], off: usize) -> f32 {
    f32::from_le_bytes(bytes[off..off + 4].try_into().expect("f32 slice"))
}

struct LookupMeta {
    count: usize,
    rows_end: usize,
}

fn lookup_meta() -> &'static LookupMeta {
    static META: OnceLock<LookupMeta> = OnceLock::new();
    META.get_or_init(|| {
        assert!(
            LOOKUP_AIRPORT_BLOB.len() >= LOOKUP_HEADER_LEN,
            "lookup airport blob too short"
        );
        assert_eq!(
            &LOOKUP_AIRPORT_BLOB[..4],
            LOOKUP_MAGIC.as_slice(),
            "lookup airport blob magic mismatch"
        );
        let count = read_u32_le(LOOKUP_AIRPORT_BLOB, 4) as usize;
        let name_blob_len = read_u32_le(LOOKUP_AIRPORT_BLOB, 8) as usize;
        let rows_end = LOOKUP_HEADER_LEN + count * LOOKUP_ROW_SIZE;
        assert_eq!(
            LOOKUP_AIRPORT_BLOB.len(),
            rows_end + name_blob_len,
            "lookup airport blob length mismatch"
        );
        LookupMeta { count, rows_end }
    })
}

fn plz_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        assert!(PLZ_BLOB.len() >= PLZ_HEADER_LEN, "plz blob too short");
        assert_eq!(
            &PLZ_BLOB[..4],
            PLZ_MAGIC.as_slice(),
            "plz blob magic mismatch"
        );
        let count = read_u32_le(PLZ_BLOB, 4) as usize;
        assert_eq!(
            PLZ_BLOB.len(),
            PLZ_HEADER_LEN + count * PLZ_ROW_SIZE,
            "plz blob length mismatch"
        );
        count
    })
}

fn decode_lookup_airport(index: usize) -> LookupAirport {
    let meta = lookup_meta();
    assert!(index < meta.count, "lookup airport index out of range");
    let row_off = LOOKUP_HEADER_LEN + index * LOOKUP_ROW_SIZE;
    let row = &LOOKUP_AIRPORT_BLOB[row_off..row_off + LOOKUP_ROW_SIZE];
    let flags = row[7];
    let lat = read_f32_le(row, 8);
    let lon = read_f32_le(row, 12);
    let name_off = read_u32_le(row, 16) as usize;
    let name_len = read_u32_le(row, 20) as usize;
    let name_bytes =
        &LOOKUP_AIRPORT_BLOB[meta.rows_end + name_off..meta.rows_end + name_off + name_len];
    let name = str::from_utf8(name_bytes).expect("airport name utf8");

    let icao = if flags & LOOKUP_FLAG_ICAO != 0 {
        Some(str::from_utf8(&row[0..4]).expect("icao utf8"))
    } else {
        None
    };
    let iata = if flags & LOOKUP_FLAG_IATA != 0 {
        Some(str::from_utf8(&row[4..7]).expect("iata utf8"))
    } else {
        None
    };

    LookupAirport {
        icao,
        iata,
        name,
        lat,
        lon,
    }
}

fn decode_plz_entry(index: usize) -> PlzEntry {
    let count = plz_count();
    assert!(index < count, "plz index out of range");
    let row_off = PLZ_HEADER_LEN + index * PLZ_ROW_SIZE;
    let row = &PLZ_BLOB[row_off..row_off + PLZ_ROW_SIZE];
    let plz = str::from_utf8(&row[0..5]).expect("plz utf8");
    let lat = read_f32_le(row, 8);
    let lon = read_f32_le(row, 12);
    PlzEntry { plz, lat, lon }
}

/// Number of packed lookup airports.
pub fn lookup_airport_count() -> usize {
    lookup_meta().count
}

/// Decode the lookup airport at `index` (0..count).
pub fn lookup_airport_at(index: usize) -> LookupAirport {
    decode_lookup_airport(index)
}

/// Iterate all packed lookup airports.
pub fn lookup_airports() -> impl Iterator<Item = LookupAirport> {
    (0..lookup_airport_count()).map(lookup_airport_at)
}

pub fn lookup_by_icao(code: &str) -> Option<LookupAirport> {
    lookup_airports().find(|a| a.icao.is_some_and(|c| c.eq_ignore_ascii_case(code)))
}

pub fn lookup_by_iata(code: &str) -> Option<LookupAirport> {
    lookup_airports().find(|a| a.iata.is_some_and(|c| c.eq_ignore_ascii_case(code)))
}

/// Number of packed PLZ entries.
pub fn plz_entry_count() -> usize {
    plz_count()
}

/// Decode the PLZ entry at `index` (0..count).
pub fn plz_entry_at(index: usize) -> PlzEntry {
    decode_plz_entry(index)
}

/// Iterate all packed PLZ entries.
pub fn plz_entries() -> impl Iterator<Item = PlzEntry> {
    (0..plz_entry_count()).map(plz_entry_at)
}

pub fn plz_coords(plz: &str) -> Option<(f32, f32)> {
    plz_entries().find(|e| e.plz == plz).map(|e| (e.lat, e.lon))
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

    #[test]
    fn berlin_plz_resolves() {
        // 10115 is Mitte — present in current plz.csv
        let (lat, lon) = plz_coords("10115").expect("10115");
        assert!((lat - 52.53).abs() < 0.2);
        assert!((lon - 13.38).abs() < 0.2);
    }

    #[test]
    fn plz_keys_are_five_digits() {
        for e in plz_entries() {
            assert_eq!(e.plz.len(), 5);
            assert!(e.plz.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn packed_blobs_are_dense() {
        // Informal floor from #388: usable airports alone ~1.8 MiB packed.
        assert!(
            LOOKUP_AIRPORT_BLOB.len() < 2_500_000,
            "{}",
            LOOKUP_AIRPORT_BLOB.len()
        );
        assert!(PLZ_BLOB.len() < 200_000, "{}", PLZ_BLOB.len());
        assert!(lookup_airport_count() > 30_000);
        assert!(plz_entry_count() > 10_000);
    }
}
