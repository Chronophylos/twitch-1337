use csv::ReaderBuilder;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const AIRPORTS_URL: &str = "https://davidmegginson.github.io/ourairports-data/airports.csv";
const RUNWAYS_URL: &str = "https://davidmegginson.github.io/ourairports-data/runways.csv";

#[derive(Debug, Deserialize)]
struct RawAirport {
    ident: String,
    #[serde(rename = "type")]
    airport_type: String,
    latitude_deg: f64,
    longitude_deg: f64,
    elevation_ft: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawRunway {
    airport_ident: String,
    length_ft: Option<String>,
    surface: Option<String>,
    closed: Option<u8>,
}

fn download_or_fallback(url: &str, fallback_path: &Path, dest_path: &Path) {
    if let Ok(body) = try_download(url) {
        fs::write(dest_path, &body).expect("failed to write downloaded CSV");
        // Also update fallback
        fs::create_dir_all(fallback_path.parent().unwrap()).ok();
        fs::write(fallback_path, &body).ok();
    } else {
        eprintln!("cargo:warning=Failed to download {url}, using fallback");
        if fallback_path.exists() {
            fs::copy(fallback_path, dest_path).expect("fallback copy failed");
        } else {
            panic!(
                "No fallback data at {} and download failed",
                fallback_path.display()
            );
        }
    }
}

fn try_download(url: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let response = ureq::get(url).call()?;
    let body = response.into_body().read_to_vec()?;
    Ok(body)
}

fn is_hard_surface(surface: &str) -> bool {
    const PREFIXES: &[&str] = &["ASP", "CON", "BIT", "PEM"];
    let upper = surface.to_ascii_uppercase();
    PREFIXES.iter().any(|p| upper.starts_with(p))
}

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ourairports_dir = manifest_dir.join("assets/flight/ourairports");

    let airports_tmp = out_dir.join("airports.csv");
    let runways_tmp = out_dir.join("runways.csv");

    download_or_fallback(
        AIRPORTS_URL,
        &ourairports_dir.join("airports.csv"),
        &airports_tmp,
    );
    download_or_fallback(
        RUNWAYS_URL,
        &ourairports_dir.join("runways.csv"),
        &runways_tmp,
    );

    // Parse runways: map airport_ident -> longest hard-surface non-closed runway length
    let mut best_runway: HashMap<String, u32> = HashMap::new();
    {
        let mut rdr = ReaderBuilder::new()
            .from_path(&runways_tmp)
            .expect("open runways.csv");
        for result in rdr.deserialize::<RawRunway>() {
            let rwy = match result {
                Ok(r) => r,
                Err(_) => continue,
            };
            if rwy.closed == Some(1) {
                continue;
            }
            if !matches!(&rwy.surface, Some(s) if is_hard_surface(s)) {
                continue;
            }
            let length: u32 = match &rwy.length_ft {
                Some(s) => match s.parse() {
                    Ok(v) => v,
                    Err(_) => continue,
                },
                None => continue,
            };
            let entry = best_runway.entry(rwy.airport_ident.clone()).or_insert(0);
            if length > *entry {
                *entry = length;
            }
        }
    }

    // Parse airports, filter, and generate code
    let mut airports = Vec::new();
    {
        let mut rdr = ReaderBuilder::new()
            .from_path(&airports_tmp)
            .expect("open airports.csv");
        for result in rdr.deserialize::<RawAirport>() {
            let apt = match result {
                Ok(a) => a,
                Err(_) => continue,
            };
            // Skip non-airports
            match apt.airport_type.as_str() {
                "large_airport" | "medium_airport" | "small_airport" => {}
                _ => continue,
            }
            // Must have ICAO-style ident (4 letters)
            if apt.ident.len() != 4 || !apt.ident.chars().all(|c| c.is_ascii_alphanumeric()) {
                continue;
            }
            // Must have a hard-surface runway
            let runway_len = match best_runway.get(&apt.ident) {
                Some(&len) => len,
                None => continue,
            };
            let elevation: i32 = apt
                .elevation_ft
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            airports.push((
                apt.ident.clone(),
                apt.latitude_deg,
                apt.longitude_deg,
                elevation,
                runway_len,
                apt.airport_type.clone(),
            ));
        }
    }

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
    println!(
        "cargo:rerun-if-changed={}",
        ourairports_dir.join("airports.csv").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ourairports_dir.join("runways.csv").display()
    );
    println!(
        "cargo:warning=Generated airport DB with {} airports",
        airports.len()
    );

    generate_aircraft_db(&out_dir, &manifest_dir);
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
