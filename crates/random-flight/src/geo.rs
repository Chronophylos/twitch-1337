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

/// Smallest absolute angle between two bearings, in degrees (0-180).
pub fn bearing_difference(a: f64, b: f64) -> f64 {
    ((a - b + 540.0).rem_euclid(360.0) - 180.0).abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const JFK: (f64, f64) = (40.6413, -73.7781);
    const LHR: (f64, f64) = (51.4700, -0.4543);

    #[test]
    fn haversine_matches_known_distances() {
        // (route, from, to, expected nm, tolerance nm)
        let cases = [
            ("JFK-LHR", JFK, LHR, 2999.0, 10.0),
            // Crosses the antimeridian.
            (
                "SFO-NRT",
                (37.6213, -122.3790),
                (35.7647, 140.3864),
                4442.0,
                10.0,
            ),
            (
                "EDDF-EDDM",
                (50.0379, 8.5622),
                (48.3537, 11.7750),
                162.0,
                5.0,
            ),
            ("same point", LHR, LHR, 0.0, 0.01),
        ];
        for (route, (lat1, lon1), (lat2, lon2), expected, tol) in cases {
            let d = haversine_distance_nm(lat1, lon1, lat2, lon2);
            assert!(
                (d - expected).abs() < tol,
                "{route} distance was {d}, expected ~{expected} nm"
            );
        }
    }

    #[test]
    fn initial_bearing_matches_known_values() {
        // (route, from, to, expected degrees, tolerance degrees)
        let cases = [
            ("JFK→LHR", JFK, LHR, 51.0, 2.0),
            // Westbound result must be normalised into 0..360.
            ("LHR→JFK", LHR, JFK, 288.0, 2.0),
            ("due north", (0.0, 0.0), (10.0, 0.0), 0.0, 0.01),
            ("due south", (10.0, 0.0), (0.0, 0.0), 180.0, 0.01),
            ("due east", (0.0, 0.0), (0.0, 10.0), 90.0, 0.01),
            ("due west", (0.0, 0.0), (0.0, -10.0), 270.0, 0.01),
        ];
        for (route, (lat1, lon1), (lat2, lon2), expected, tol) in cases {
            let b = initial_bearing(lat1, lon1, lat2, lon2);
            assert!(
                (b - expected).abs() < tol,
                "{route} bearing was {b}, expected ~{expected}°"
            );
        }
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
    fn cardinal_negative_and_out_of_range() {
        assert_eq!(cardinal_direction(-45.0), "NW");
        assert_eq!(cardinal_direction(-90.0), "W");
        assert_eq!(cardinal_direction(-180.0), "S");
        assert_eq!(cardinal_direction(360.0), "N");
        assert_eq!(cardinal_direction(450.0), "E");
    }

    #[test]
    fn bearing_difference_wraps_around_north() {
        assert!((bearing_difference(350.0, 10.0) - 20.0).abs() < 1e-9);
        assert!((bearing_difference(10.0, 350.0) - 20.0).abs() < 1e-9);
        assert!((bearing_difference(0.0, 180.0) - 180.0).abs() < 1e-9);
        assert!((bearing_difference(90.0, 90.0)).abs() < 1e-9);
    }
}
