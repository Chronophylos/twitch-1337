#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "build script: a panic is a build failure on committed data"
)]
#[path = "src/bake.rs"]
mod bake;

use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn report_counts(path: &Path, counts: bake::RowCounts) {
    println!(
        "cargo:warning={}: kept={}, filtered={}, malformed={}",
        path.display(),
        counts.kept,
        counts.filtered,
        counts.malformed
    );
}

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ourairports_dir = manifest_dir.join("assets/flight/ourairports");

    let airports_csv = ourairports_dir.join("airports.csv");
    let runways_csv = ourairports_dir.join("runways.csv");

    // Parse runways: map airport_ident -> longest hard-surface non-closed runway length.
    let parsed_runways = bake::read_runways(&runways_csv).unwrap_or_else(|error| panic!("{error}"));
    report_counts(&runways_csv, parsed_runways.counts);
    let best_runway = parsed_runways.rows;

    // Parse airports, filter deliberately, and reject excessive malformed rows.
    let parsed_airports =
        bake::read_airports(&airports_csv, &best_runway).unwrap_or_else(|error| panic!("{error}"));
    report_counts(&airports_csv, parsed_airports.counts);
    let airports = parsed_airports.rows;

    // Generate Rust source
    let gen_path = out_dir.join("airport_db.rs");
    let mut f = fs::File::create(&gen_path).expect("create airport_db.rs");

    writeln!(f, "static AIRPORTS: &[Airport] = &[").unwrap();
    for (icao, lat, lon, elev, rwy, airport_type) in &airports {
        let size_variant = match airport_type.as_str() {
            "large_airport" => "Large",
            "medium_airport" => "Medium",
            "small_airport" => "Small",
            _ => unreachable!(),
        };
        writeln!(
            f,
            "    Airport {{ icao: \"{icao}\", latitude: {lat}_f64, longitude: {lon}_f64, elevation_ft: {elev}, runway_length_ft: {rwy}, size: AirportSize::{size_variant} }},"
        )
        .unwrap();
    }
    writeln!(f, "];").unwrap();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", airports_csv.display());
    println!("cargo:rerun-if-changed={}", runways_csv.display());

    generate_aircraft_db(&out_dir, &manifest_dir);
    generate_lookup_airport_db(&out_dir, &manifest_dir);
    generate_plz_db(&out_dir, &manifest_dir);
}

// ── Aircraft TOML parsing ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AircraftToml {
    aircraft: AircraftMeta,
    performance: PerformanceToml,
    fuel: FuelToml,
}

#[derive(Debug, Deserialize)]
struct AircraftMeta {
    name: String,
    // ICAO aircraft type designator, not an airport code; digits are valid.
    icao_type: String,
}

#[derive(Debug, Deserialize)]
struct PerformanceToml {
    cruise_speed_ktas: u16,
    cruise_altitude_ft: u32,
    service_ceiling_ft: u32,
    min_runway_length_ft: u32,
    climb: PhaseToml,
    descent: PhaseToml,
}

#[derive(Debug, Deserialize)]
struct PhaseToml {
    speed_ktas: u16,
    rate_fpm: u16,
}

#[derive(Debug, Deserialize)]
struct FuelToml {
    capacity_kg: f64,
    fuel_type: String,
    flow: FuelFlowToml,
}

#[derive(Debug, Deserialize)]
struct FuelFlowToml {
    climb_kg_per_hour: f64,
    cruise_kg_per_hour: f64,
    descent_kg_per_hour: f64,
}

fn generate_aircraft_db(out_dir: &Path, manifest_dir: &Path) {
    let aircraft_dir = manifest_dir.join("assets/flight/aircraft");
    println!("cargo:rerun-if-changed={}", aircraft_dir.display());

    let mut entries: Vec<_> = fs::read_dir(&aircraft_dir)
        .unwrap_or_else(|e| panic!("Cannot read {}: {e}", aircraft_dir.display()))
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    entries.sort();

    let mut seen_icao: HashSet<String> = HashSet::new();
    let mut aircraft_list: Vec<AircraftToml> = Vec::new();

    for path in &entries {
        println!("cargo:rerun-if-changed={}", path.display());

        let contents = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("Cannot read {}: {e}", path.display()));
        let parsed: AircraftToml = toml::from_str(&contents)
            .unwrap_or_else(|e| panic!("Invalid TOML in {}: {e}", path.display()));

        let filename_stem = path.file_stem().unwrap().to_str().unwrap();
        let expected_stem = parsed.aircraft.icao_type.to_lowercase();
        assert_eq!(
            filename_stem, expected_stem,
            "Filename {filename_stem}.toml does not match icao_type {}",
            parsed.aircraft.icao_type
        );

        assert!(
            !seen_icao.contains(&parsed.aircraft.icao_type),
            "Duplicate icao_type: {}",
            parsed.aircraft.icao_type
        );
        assert!(
            parsed
                .aircraft
                .icao_type
                .chars()
                .all(|c| c.is_ascii_alphanumeric()),
            "{}: icao_type must be ASCII alphanumeric, got {:?}",
            filename_stem,
            parsed.aircraft.icao_type
        );
        seen_icao.insert(parsed.aircraft.icao_type.clone());

        assert!(
            parsed.performance.service_ceiling_ft >= parsed.performance.cruise_altitude_ft,
            "{}: service_ceiling_ft must be >= cruise_altitude_ft",
            parsed.aircraft.icao_type
        );

        assert!(
            parsed.fuel.fuel_type == "jet" || parsed.fuel.fuel_type == "avgas",
            "{}: fuel_type must be \"jet\" or \"avgas\", got \"{}\"",
            parsed.aircraft.icao_type,
            parsed.fuel.fuel_type
        );

        assert!(
            parsed.performance.cruise_speed_ktas > 0,
            "{}: cruise_speed must be positive",
            parsed.aircraft.icao_type
        );
        assert!(
            parsed.performance.climb.speed_ktas > 0,
            "{}: climb_speed must be positive",
            parsed.aircraft.icao_type
        );
        assert!(
            parsed.performance.climb.rate_fpm > 0,
            "{}: climb_rate must be positive",
            parsed.aircraft.icao_type
        );
        assert!(
            parsed.performance.descent.speed_ktas > 0,
            "{}: descent_speed must be positive",
            parsed.aircraft.icao_type
        );
        assert!(
            parsed.performance.descent.rate_fpm > 0,
            "{}: descent_rate must be positive",
            parsed.aircraft.icao_type
        );
        assert!(
            parsed.fuel.capacity_kg > 0.0,
            "{}: fuel_capacity must be positive",
            parsed.aircraft.icao_type
        );
        assert!(
            parsed.fuel.flow.climb_kg_per_hour > 0.0,
            "{}: fuel_flow_climb must be positive",
            parsed.aircraft.icao_type
        );
        assert!(
            parsed.fuel.flow.cruise_kg_per_hour > 0.0,
            "{}: fuel_flow_cruise must be positive",
            parsed.aircraft.icao_type
        );
        assert!(
            parsed.fuel.flow.descent_kg_per_hour > 0.0,
            "{}: fuel_flow_descent must be positive",
            parsed.aircraft.icao_type
        );

        aircraft_list.push(parsed);
    }

    // Generate Rust source
    let gen_path = out_dir.join("aircraft_db.rs");
    let mut f = fs::File::create(&gen_path).expect("create aircraft_db.rs");

    writeln!(f, "static AIRCRAFT_DB: &[Aircraft] = &[").unwrap();
    for ac in &aircraft_list {
        let fuel_type_variant = match ac.fuel.fuel_type.as_str() {
            "jet" => "Jet",
            "avgas" => "Avgas",
            _ => unreachable!(),
        };
        let escaped_name = ac
            .aircraft
            .name
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>();
        writeln!(f, "    Aircraft {{").unwrap();
        writeln!(f, "        name: \"{escaped_name}\",").unwrap();
        writeln!(f, "        icao_type: \"{}\",", ac.aircraft.icao_type).unwrap();
        writeln!(
            f,
            "        cruise_speed_ktas: {},",
            ac.performance.cruise_speed_ktas
        )
        .unwrap();
        writeln!(
            f,
            "        cruise_altitude_ft: {},",
            ac.performance.cruise_altitude_ft
        )
        .unwrap();
        writeln!(
            f,
            "        service_ceiling_ft: {},",
            ac.performance.service_ceiling_ft
        )
        .unwrap();
        writeln!(
            f,
            "        min_runway_length_ft: {},",
            ac.performance.min_runway_length_ft
        )
        .unwrap();
        writeln!(
            f,
            "        climb_speed_ktas: {},",
            ac.performance.climb.speed_ktas
        )
        .unwrap();
        writeln!(
            f,
            "        climb_rate_fpm: {},",
            ac.performance.climb.rate_fpm
        )
        .unwrap();
        writeln!(
            f,
            "        descent_speed_ktas: {},",
            ac.performance.descent.speed_ktas
        )
        .unwrap();
        writeln!(
            f,
            "        descent_rate_fpm: {},",
            ac.performance.descent.rate_fpm
        )
        .unwrap();
        writeln!(f, "        fuel_capacity_kg: {:.1},", ac.fuel.capacity_kg).unwrap();
        writeln!(f, "        fuel_type: FuelType::{fuel_type_variant},").unwrap();
        writeln!(
            f,
            "        fuel_flow_climb_kg_per_hour: {:.1},",
            ac.fuel.flow.climb_kg_per_hour
        )
        .unwrap();
        writeln!(
            f,
            "        fuel_flow_cruise_kg_per_hour: {:.1},",
            ac.fuel.flow.cruise_kg_per_hour
        )
        .unwrap();
        writeln!(
            f,
            "        fuel_flow_descent_kg_per_hour: {:.1},",
            ac.fuel.flow.descent_kg_per_hour
        )
        .unwrap();
        writeln!(f, "    }},").unwrap();
    }
    writeln!(f, "];").unwrap();

    println!(
        "cargo:warning=Generated aircraft DB with {} aircraft",
        aircraft_list.len()
    );
}

/// Packed lookup-airport blob (`LKA1`):
/// header (12) + rows (`count` × 24) + UTF-8 name blob.
/// Row: icao[4] iata[3] flags lat_le lon_le name_off_le name_len_le.
const LOOKUP_AIRPORT_ROW_SIZE: usize = 24;
const LOOKUP_FLAG_ICAO: u8 = 1;
const LOOKUP_FLAG_IATA: u8 = 2;

/// Packed PLZ blob (`PLZ1`): header (8) + rows (`count` × 16).
/// Row: plz[5] pad[3] lat_le lon_le.
const PLZ_ROW_SIZE: usize = 16;

fn generate_lookup_airport_db(out_dir: &Path, manifest_dir: &Path) {
    let path = manifest_dir.join("assets/location/airports.csv");
    println!("cargo:rerun-if-changed={}", path.display());

    let parsed = bake::read_lookup_airports(&path).unwrap_or_else(|error| panic!("{error}"));
    report_counts(&path, parsed.counts);

    let mut rows = Vec::new();
    let mut name_blob = Vec::new();
    for row in parsed.rows {
        let name_off = u32::try_from(name_blob.len()).expect("name blob offset fits u32");
        let name_len = u32::try_from(row.name.len()).expect("name length fits u32");
        name_blob.extend_from_slice(row.name.as_bytes());

        let mut flags = 0;
        if row.has_ident {
            flags |= LOOKUP_FLAG_ICAO;
        }
        if row.has_iata {
            flags |= LOOKUP_FLAG_IATA;
        }
        rows.push((
            row.ident, row.iata, flags, row.lat, row.lon, name_off, name_len,
        ));
    }

    let count = u32::try_from(rows.len()).expect("airport count fits u32");
    let name_blob_len = u32::try_from(name_blob.len()).expect("name blob len fits u32");
    let mut blob = Vec::with_capacity(12 + rows.len() * LOOKUP_AIRPORT_ROW_SIZE + name_blob.len());
    blob.extend_from_slice(b"LKA1");
    blob.extend_from_slice(&count.to_le_bytes());
    blob.extend_from_slice(&name_blob_len.to_le_bytes());
    for (icao, iata, flags, lat, lon, name_off, name_len) in &rows {
        blob.extend_from_slice(icao);
        blob.extend_from_slice(iata);
        blob.push(*flags);
        blob.extend_from_slice(&lat.to_le_bytes());
        blob.extend_from_slice(&lon.to_le_bytes());
        blob.extend_from_slice(&name_off.to_le_bytes());
        blob.extend_from_slice(&name_len.to_le_bytes());
    }
    blob.extend_from_slice(&name_blob);

    let bin_path = out_dir.join("location_airports.bin");
    #[expect(
        clippy::disallowed_methods,
        reason = "build-script output under OUT_DIR, not runtime persistence"
    )]
    fs::write(&bin_path, &blob).expect("write location_airports.bin");
}

fn generate_plz_db(out_dir: &Path, manifest_dir: &Path) {
    let path = manifest_dir.join("assets/location/plz.csv");
    println!("cargo:rerun-if-changed={}", path.display());

    let parsed = bake::read_plz(&path).unwrap_or_else(|error| panic!("{error}"));
    report_counts(&path, parsed.counts);
    let rows = parsed.rows;

    let count = u32::try_from(rows.len()).expect("plz count fits u32");
    let mut blob = Vec::with_capacity(8 + rows.len() * PLZ_ROW_SIZE);
    blob.extend_from_slice(b"PLZ1");
    blob.extend_from_slice(&count.to_le_bytes());
    for row in &rows {
        blob.extend_from_slice(&row.plz);
        blob.extend_from_slice(&[0; 3]);
        blob.extend_from_slice(&row.lat.to_le_bytes());
        blob.extend_from_slice(&row.lon.to_le_bytes());
    }

    let bin_path = out_dir.join("plz.bin");
    #[expect(
        clippy::disallowed_methods,
        reason = "build-script output under OUT_DIR, not runtime persistence"
    )]
    fs::write(&bin_path, &blob).expect("write plz.bin");
}
