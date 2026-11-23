//! CSV parsing and row accounting shared by the build script and its tests.
//!
//! OurAirports `ident` is a local identifier, not necessarily an ICAO airport
//! code: values such as `00AA` legitimately contain digits. The separate
//! OurAirports `icao_code` column is the actual ICAO code and, when present,
//! must contain exactly four ASCII uppercase letters. The headerless location
//! airport CSV also stores OurAirports `ident` in its first column, despite the
//! lookup API's historical `icao` naming, so it follows the `ident` rules too.

use csv::{ReaderBuilder, StringRecord};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RowCounts {
    pub kept: usize,
    pub filtered: usize,
    pub malformed: usize,
}

impl RowCounts {
    fn total(self) -> usize {
        self.kept + self.filtered + self.malformed
    }

    fn finish<T>(self, file: &Path, rows: T) -> Result<ParsedCsv<T>, String> {
        // Integer arithmetic avoids rounding around the threshold. For integer
        // counts this is equivalent to malformed * 100 > total.
        if self.malformed > self.total() / 100 {
            return Err(format!(
                "{}: malformed rows exceed 1% (kept={}, filtered={}, malformed={}, total={})",
                file.display(),
                self.kept,
                self.filtered,
                self.malformed,
                self.total()
            ));
        }
        Ok(ParsedCsv { rows, counts: self })
    }
}

#[derive(Debug)]
pub struct ParsedCsv<T> {
    pub rows: T,
    pub counts: RowCounts,
}

fn require_columns(file: &Path, headers: &StringRecord, required: &[&str]) -> Result<(), String> {
    for &column in required {
        if !headers.iter().any(|header| header == column) {
            return Err(format!(
                "{}: missing required column `{column}`",
                file.display()
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct RawRunway {
    airport_ident: String,
    length_ft: Option<String>,
    surface: Option<String>,
    closed: Option<u8>,
}

fn is_hard_surface(surface: &str) -> bool {
    const PREFIXES: &[&str] = &["ASP", "CON", "BIT", "PEM"];
    let upper = surface.to_ascii_uppercase();
    PREFIXES.iter().any(|p| upper.starts_with(p))
}

#[cfg(not(test))]
pub fn read_runways(file: &Path) -> Result<ParsedCsv<HashMap<String, u32>>, String> {
    let input = std::fs::File::open(file)
        .map_err(|error| format!("{}: open CSV: {error}", file.display()))?;
    parse_runways(input, file)
}

fn parse_runways<R: Read>(
    input: R,
    file: &Path,
) -> Result<ParsedCsv<HashMap<String, u32>>, String> {
    let mut rdr = ReaderBuilder::new().from_reader(input);
    let headers = rdr
        .headers()
        .map_err(|error| format!("{}: read CSV headers: {error}", file.display()))?
        .clone();
    require_columns(
        file,
        &headers,
        &["airport_ident", "length_ft", "surface", "closed"],
    )?;

    // Map airport_ident -> longest hard-surface non-closed runway length.
    let mut best_runway = HashMap::new();
    let mut counts = RowCounts::default();
    for result in rdr.deserialize::<RawRunway>() {
        let Ok(rwy) = result else {
            counts.malformed += 1;
            continue;
        };
        if rwy.airport_ident.trim().is_empty() {
            counts.malformed += 1;
            continue;
        }
        if rwy.closed == Some(1) {
            counts.filtered += 1;
            continue;
        }
        if !matches!(&rwy.surface, Some(surface) if is_hard_surface(surface)) {
            counts.filtered += 1;
            continue;
        }
        let length = match &rwy.length_ft {
            Some(value) => match value.parse::<u32>() {
                Ok(length) => length,
                Err(_) => {
                    counts.malformed += 1;
                    continue;
                }
            },
            None => {
                counts.malformed += 1;
                continue;
            }
        };
        let entry = best_runway.entry(rwy.airport_ident).or_insert(0);
        if length > *entry {
            *entry = length;
        }
        counts.kept += 1;
    }
    counts.finish(file, best_runway)
}

#[derive(Debug, Deserialize)]
struct RawAirport {
    // Local OurAirports identifier, not necessarily an ICAO code.
    ident: String,
    #[serde(rename = "type")]
    airport_type: String,
    latitude_deg: f64,
    longitude_deg: f64,
    elevation_ft: Option<String>,
    // Actual ICAO airport code; legitimately empty for most small airports.
    icao_code: Option<String>,
}

pub type AirportRows = Vec<(String, f64, f64, i32, u32, String)>;

fn is_ourairports_ident(ident: &str) -> bool {
    ident.len() == 4 && ident.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn is_icao_code(code: &str) -> bool {
    code.len() == 4 && code.bytes().all(|byte| byte.is_ascii_uppercase())
}

#[cfg(not(test))]
pub fn read_airports(
    file: &Path,
    best_runway: &HashMap<String, u32>,
) -> Result<ParsedCsv<AirportRows>, String> {
    let input = std::fs::File::open(file)
        .map_err(|error| format!("{}: open CSV: {error}", file.display()))?;
    parse_airports(input, file, best_runway)
}

fn parse_airports<R: Read>(
    input: R,
    file: &Path,
    best_runway: &HashMap<String, u32>,
) -> Result<ParsedCsv<AirportRows>, String> {
    let mut rdr = ReaderBuilder::new().from_reader(input);
    let headers = rdr
        .headers()
        .map_err(|error| format!("{}: read CSV headers: {error}", file.display()))?
        .clone();
    require_columns(
        file,
        &headers,
        &[
            "ident",
            "type",
            "latitude_deg",
            "longitude_deg",
            "elevation_ft",
            "icao_code",
        ],
    )?;

    let mut airports = Vec::new();
    let mut counts = RowCounts::default();
    for result in rdr.deserialize::<RawAirport>() {
        let Ok(apt) = result else {
            counts.malformed += 1;
            continue;
        };
        if apt.ident.trim().is_empty() {
            counts.malformed += 1;
            continue;
        }
        if apt
            .icao_code
            .as_deref()
            .is_some_and(|code| !is_icao_code(code))
        {
            counts.malformed += 1;
            continue;
        }
        match apt.airport_type.as_str() {
            "large_airport" | "medium_airport" | "small_airport" => {}
            _ => {
                counts.filtered += 1;
                continue;
            }
        }
        if !is_ourairports_ident(&apt.ident) {
            counts.filtered += 1;
            continue;
        }
        let Some(&runway_len) = best_runway.get(&apt.ident) else {
            counts.filtered += 1;
            continue;
        };
        let elevation = apt
            .elevation_ft
            .as_deref()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);

        airports.push((
            apt.ident,
            apt.latitude_deg,
            apt.longitude_deg,
            elevation,
            runway_len,
            apt.airport_type,
        ));
        counts.kept += 1;
    }
    counts.finish(file, airports)
}

#[derive(Debug)]
pub struct LookupAirportRow {
    pub ident: [u8; 4],
    pub iata: [u8; 3],
    pub has_ident: bool,
    pub has_iata: bool,
    pub lat: f32,
    pub lon: f32,
    pub name: String,
}

fn sanitize_lookup_name(name: &str) -> String {
    name.chars()
        .filter(|c| {
            !c.is_control()
                && *c != '\u{00ad}'
                && *c != '\u{200b}'
                && *c != '\u{200c}'
                && *c != '\u{200d}'
                && *c != '\u{feff}'
        })
        .collect()
}

#[cfg(not(test))]
pub fn read_lookup_airports(file: &Path) -> Result<ParsedCsv<Vec<LookupAirportRow>>, String> {
    let input = std::fs::File::open(file)
        .map_err(|error| format!("{}: open CSV: {error}", file.display()))?;
    parse_lookup_airports(input, file)
}

fn parse_lookup_airports<R: Read>(
    input: R,
    file: &Path,
) -> Result<ParsedCsv<Vec<LookupAirportRow>>, String> {
    let mut rdr = ReaderBuilder::new().has_headers(false).from_reader(input);
    let mut rows = Vec::new();
    let mut counts = RowCounts::default();
    for result in rdr.records() {
        let Ok(rec) = result else {
            counts.malformed += 1;
            continue;
        };
        if rec.len() < 5 {
            counts.malformed += 1;
            continue;
        }
        // This is OurAirports `ident`, not the separate `icao_code` column.
        // Keep accepting digit-containing local identifiers such as `00AA`.
        let ident = rec[0].trim();
        let iata = rec[1].trim();
        let name = sanitize_lookup_name(rec[2].trim());
        let Ok(lat) = rec[3].trim().parse::<f32>() else {
            counts.malformed += 1;
            continue;
        };
        let Ok(lon) = rec[4].trim().parse::<f32>() else {
            counts.malformed += 1;
            continue;
        };

        let has_ident = ident.len() == 4;
        let has_iata = iata.len() == 3;
        if !has_ident && !has_iata {
            counts.filtered += 1;
            continue;
        }

        let mut ident_bytes = [0; 4];
        let mut iata_bytes = [0; 3];
        if has_ident {
            ident_bytes.copy_from_slice(ident.as_bytes());
        }
        if has_iata {
            iata_bytes.copy_from_slice(iata.as_bytes());
        }
        rows.push(LookupAirportRow {
            ident: ident_bytes,
            iata: iata_bytes,
            has_ident,
            has_iata,
            lat,
            lon,
            name,
        });
        counts.kept += 1;
    }
    counts.finish(file, rows)
}

#[derive(Debug)]
pub struct PlzRow {
    pub plz: [u8; 5],
    pub lat: f32,
    pub lon: f32,
}

#[cfg(not(test))]
pub fn read_plz(file: &Path) -> Result<ParsedCsv<Vec<PlzRow>>, String> {
    let input = std::fs::File::open(file)
        .map_err(|error| format!("{}: open CSV: {error}", file.display()))?;
    parse_plz(input, file)
}

fn parse_plz<R: Read>(input: R, file: &Path) -> Result<ParsedCsv<Vec<PlzRow>>, String> {
    let mut rdr = ReaderBuilder::new().has_headers(false).from_reader(input);
    let mut rows = Vec::new();
    let mut counts = RowCounts::default();
    for result in rdr.records() {
        let Ok(rec) = result else {
            counts.malformed += 1;
            continue;
        };
        if rec.len() < 3 {
            counts.malformed += 1;
            continue;
        }
        let plz = rec[0].trim();
        if plz.len() != 5 || !plz.bytes().all(|byte| byte.is_ascii_digit()) {
            counts.malformed += 1;
            continue;
        }
        let Ok(lat) = rec[1].trim().parse::<f32>() else {
            counts.malformed += 1;
            continue;
        };
        let Ok(lon) = rec[2].trim().parse::<f32>() else {
            counts.malformed += 1;
            continue;
        };
        let mut plz_bytes = [0; 5];
        plz_bytes.copy_from_slice(plz.as_bytes());
        rows.push(PlzRow {
            plz: plz_bytes,
            lat,
            lon,
        });
        counts.kept += 1;
    }
    counts.finish(file, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;

    fn runway_csv(total: usize, malformed: usize) -> String {
        let mut csv = String::from("airport_ident,length_ft,surface,closed\n");
        for index in 0..total {
            let length = if index < malformed { "" } else { "3000" };
            writeln!(csv, "K{index:03},{length},CON,0").unwrap();
        }
        csv
    }

    #[test]
    fn missing_required_column_fails_with_file_and_column() {
        let csv = "airport_ident,surface,closed\nEDDF,CON,0\n";
        let error = parse_runways(csv.as_bytes(), Path::new("fixtures/runways.csv")).unwrap_err();
        assert_eq!(
            error,
            "fixtures/runways.csv: missing required column `length_ft`"
        );
    }

    #[test]
    fn two_percent_malformed_fails() {
        let error = parse_runways(
            runway_csv(100, 2).as_bytes(),
            Path::new("fixtures/runways.csv"),
        )
        .unwrap_err();
        assert!(error.contains("malformed rows exceed 1%"), "{error}");
        assert!(
            error.contains("kept=98, filtered=0, malformed=2, total=100"),
            "{error}"
        );
    }

    #[test]
    fn half_percent_malformed_passes_with_counts() {
        let parsed = parse_runways(
            runway_csv(200, 1).as_bytes(),
            Path::new("fixtures/runways.csv"),
        )
        .unwrap();
        assert_eq!(
            parsed.counts,
            RowCounts {
                kept: 199,
                filtered: 0,
                malformed: 1,
            }
        );
        assert_eq!(parsed.rows.len(), 199);
    }

    #[test]
    fn deliberate_runway_filtering_is_not_malformed() {
        let csv = "airport_ident,length_ft,surface,closed\n\
                   CLOSED,3000,CON,1\n\
                   GRASS,,GRASS,0\n\
                   VALID,3000,CON,0\n";
        let parsed = parse_runways(csv.as_bytes(), Path::new("fixtures/runways.csv")).unwrap();
        assert_eq!(
            parsed.counts,
            RowCounts {
                kept: 1,
                filtered: 2,
                malformed: 0,
            }
        );
        assert_eq!(parsed.rows.get("VALID"), Some(&3000));
        assert!(!parsed.rows.contains_key("CLOSED"));
        assert!(!parsed.rows.contains_key("GRASS"));
    }

    #[test]
    fn local_identities_are_not_validated_as_icao_codes() {
        assert!(is_ourairports_ident("00AA"));
        assert!(is_ourairports_ident("K1A2"));
        assert!(!is_icao_code("00AA"));
        assert!(!is_icao_code("K1A2"));
        assert!(!is_icao_code("eddf"));
        assert!(is_icao_code("EDDF"));

        let csv = "ident,type,latitude_deg,longitude_deg,elevation_ft,icao_code\n\
                   00AA,small_airport,1,2,,\n";
        let runways = HashMap::from([(String::from("00AA"), 3000)]);
        let parsed =
            parse_airports(csv.as_bytes(), Path::new("fixtures/airports.csv"), &runways).unwrap();
        assert_eq!(parsed.counts.kept, 1);
        assert_eq!(parsed.rows[0].0, "00AA");
    }

    #[test]
    fn malformed_actual_icao_code_is_rejected() {
        let csv = "ident,type,latitude_deg,longitude_deg,elevation_ft,icao_code\n\
                   EDDF,large_airport,50,8,100,ED1F\n";
        let runways = HashMap::from([(String::from("EDDF"), 10_000)]);
        let error = parse_airports(csv.as_bytes(), Path::new("fixtures/airports.csv"), &runways)
            .unwrap_err();
        assert!(
            error.contains("kept=0, filtered=0, malformed=1, total=1"),
            "{error}"
        );
    }

    #[test]
    fn lookup_csv_preserves_digit_containing_identities() {
        let csv = "00AA,,Local Airfield,1,2\n\
                   EDDF,FRA,Frankfurt am Main Airport,50,8\n\
                   NO_CODE,,No usable code,3,4\n";
        let parsed =
            parse_lookup_airports(csv.as_bytes(), Path::new("fixtures/location-airports.csv"))
                .unwrap();
        assert_eq!(
            parsed.counts,
            RowCounts {
                kept: 2,
                filtered: 1,
                malformed: 0,
            }
        );
        assert!(parsed.rows[0].has_ident);
        assert_eq!(parsed.rows[0].ident, *b"00AA");
        assert!(!parsed.rows[0].has_iata);
        assert_eq!(parsed.rows[0].iata, [0; 3]);
        assert_eq!(parsed.rows[0].lat, 1.0);
        assert_eq!(parsed.rows[0].lon, 2.0);
        assert_eq!(parsed.rows[0].name, "Local Airfield");
        assert!(parsed.rows[1].has_iata);
        assert_eq!(parsed.rows[1].iata, *b"FRA");
    }

    #[test]
    fn valid_plz_is_kept_with_its_coordinates() {
        let csv = "01053,51.0,13.7\n";
        let parsed = parse_plz(csv.as_bytes(), Path::new("fixtures/plz.csv")).unwrap();
        assert_eq!(parsed.counts.kept, 1);
        assert_eq!(parsed.rows[0].plz, *b"01053");
        assert_eq!(parsed.rows[0].lat, 51.0);
        assert_eq!(parsed.rows[0].lon, 13.7);
    }

    #[test]
    fn invalid_plz_is_malformed_instead_of_a_deliberate_filter() {
        let csv = "1234,51.0,13.7\n";
        let error = parse_plz(csv.as_bytes(), Path::new("fixtures/plz.csv")).unwrap_err();
        assert!(
            error.contains("kept=0, filtered=0, malformed=1, total=1"),
            "{error}"
        );
    }
}
