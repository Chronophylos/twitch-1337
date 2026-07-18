// Geographic calculations: distance, bearing, and compass directions

/// Calculates great-circle distance in nautical miles between two points.
pub fn haversine_distance_nm(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_RADIUS_NM: f64 = 3440.065; // mean Earth radius in nautical miles

    let lat1 = lat1.to_radians();
    let lat2 = lat2.to_radians();
    let dlat = lat2 - lat1;
    let dlon = (lon2 - lon1).to_radians();

    let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();

    EARTH_RADIUS_NM * c
}

/// Returns the initial bearing (forward azimuth) in degrees (0-360) from point 1 to point 2.
pub fn initial_bearing(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let lat1 = lat1.to_radians();
    let lat2 = lat2.to_radians();
    let dlon = (lon2 - lon1).to_radians();

    let x = dlon.sin() * lat2.cos();
    let y = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();

    (x.atan2(y).to_degrees() + 360.0) % 360.0
}

/// Converts a bearing (0-360) to an 8-point compass label.
pub fn cardinal_direction(bearing: f64) -> &'static str {
    const DIRECTIONS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    let index = ((bearing + 22.5).rem_euclid(360.0) / 45.0) as usize;
    DIRECTIONS[index]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_distance_jfk_to_lhr() {
        // JFK (40.6413, -73.7781) to LHR (51.4700, -0.4543)
        // Known great-circle distance: ~2999 nm
        let d = haversine_distance_nm(40.6413, -73.7781, 51.4700, -0.4543);
        assert!(
            (d - 2999.0).abs() < 10.0,
            "JFK-LHR distance was {d}, expected ~2999 nm"
        );
    }

    #[test]
    fn known_distance_sfo_to_nrt() {
        // SFO (37.6213, -122.3790) to NRT (35.7647, 140.3864)
        // Haversine great-circle distance: ~4442 nm
        let d = haversine_distance_nm(37.6213, -122.3790, 35.7647, 140.3864);
        assert!(
            (d - 4442.0).abs() < 10.0,
            "SFO-NRT distance was {d}, expected ~4442 nm"
        );
    }

    #[test]
    fn zero_distance_same_point() {
        let d = haversine_distance_nm(51.4700, -0.4543, 51.4700, -0.4543);
        assert!(d.abs() < 0.01, "Same point distance should be ~0, was {d}");
    }

    #[test]
    fn short_distance_eddf_to_eddm() {
        // Frankfurt (50.0379, 8.5622) to Munich (48.3537, 11.7750)
        // Haversine great-circle distance: ~162 nm
        let d = haversine_distance_nm(50.0379, 8.5622, 48.3537, 11.7750);
        assert!(
            (d - 162.0).abs() < 5.0,
            "EDDF-EDDM distance was {d}, expected ~162 nm"
        );
    }

    #[test]
    fn bearing_jfk_to_lhr() {
        // JFK to LHR: initial bearing ~51° (northeast)
        let b = initial_bearing(40.6413, -73.7781, 51.4700, -0.4543);
        assert!(
            (b - 51.0).abs() < 2.0,
            "JFK→LHR bearing was {b}, expected ~51°"
        );
    }

    #[test]
    fn bearing_lhr_to_jfk() {
        // LHR to JFK: initial bearing ~288° (west-northwest)
        let b = initial_bearing(51.4700, -0.4543, 40.6413, -73.7781);
        assert!(
            (b - 288.0).abs() < 2.0,
            "LHR→JFK bearing was {b}, expected ~288°"
        );
    }

    #[test]
    fn bearing_due_north() {
        let b = initial_bearing(0.0, 0.0, 10.0, 0.0);
        assert!(
            (b - 0.0).abs() < 0.01,
            "Due north bearing was {b}, expected 0°"
        );
    }

    #[test]
    fn bearing_due_south() {
        let b = initial_bearing(10.0, 0.0, 0.0, 0.0);
        assert!(
            (b - 180.0).abs() < 0.01,
            "Due south bearing was {b}, expected 180°"
        );
    }

    #[test]
    fn bearing_due_east() {
        // Along the equator, due east
        let b = initial_bearing(0.0, 0.0, 0.0, 10.0);
        assert!(
            (b - 90.0).abs() < 0.01,
            "Due east bearing was {b}, expected 90°"
        );
    }

    #[test]
    fn bearing_due_west() {
        let b = initial_bearing(0.0, 0.0, 0.0, -10.0);
        assert!(
            (b - 270.0).abs() < 0.01,
            "Due west bearing was {b}, expected 270°"
        );
    }

    #[test]
    fn bearing_same_point() {
        // Bearing is undefined for same point; just ensure no panic and result is finite
        let b = initial_bearing(51.4700, -0.4543, 51.4700, -0.4543);
        assert!(
            b.is_finite(),
            "Same-point bearing should be finite, was {b}"
        );
    }

    #[test]
    fn cardinal_boundaries() {
        assert_eq!(cardinal_direction(0.0), "N");
        assert_eq!(cardinal_direction(22.4), "N");
        assert_eq!(cardinal_direction(22.5), "NE");
        assert_eq!(cardinal_direction(45.0), "NE");
        assert_eq!(cardinal_direction(90.0), "E");
        assert_eq!(cardinal_direction(135.0), "SE");
        assert_eq!(cardinal_direction(180.0), "S");
        assert_eq!(cardinal_direction(225.0), "SW");
        assert_eq!(cardinal_direction(270.0), "W");
        assert_eq!(cardinal_direction(315.0), "NW");
        assert_eq!(cardinal_direction(359.9), "N");
    }

    #[test]
    fn cardinal_mid_sectors() {
        assert_eq!(cardinal_direction(10.0), "N");
        assert_eq!(cardinal_direction(60.0), "NE");
        assert_eq!(cardinal_direction(100.0), "E");
        assert_eq!(cardinal_direction(150.0), "SE");
        assert_eq!(cardinal_direction(200.0), "S");
        assert_eq!(cardinal_direction(240.0), "SW");
        assert_eq!(cardinal_direction(300.0), "NW");
        assert_eq!(cardinal_direction(340.0), "N");
    }

    #[test]
    fn cardinal_negative_and_out_of_range() {
        assert_eq!(cardinal_direction(-45.0), "NW");
        assert_eq!(cardinal_direction(-90.0), "W");
        assert_eq!(cardinal_direction(-180.0), "S");
        assert_eq!(cardinal_direction(360.0), "N");
        assert_eq!(cardinal_direction(450.0), "E");
    }
}
