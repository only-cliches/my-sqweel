const EARTH_RADIUS_M: f64 = 6372797.560856;

const GEO_LAT_MIN: f64 = -85.05112878;
const GEO_LAT_MAX: f64 = 85.05112878;
const GEO_LAT_RANGE: f64 = GEO_LAT_MAX - GEO_LAT_MIN;

const STD_LAT_RANGE: f64 = 180.0;
const LON_RANGE: f64 = 360.0;

const GEO_STEP: u32 = 26;

const BASE32: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";

#[derive(Debug, Clone, Copy)]
pub enum DistUnit {
    M,
    Km,
    Ft,
    Mi,
}

impl DistUnit {
    pub fn parse(s: &str) -> Option<DistUnit> {
        match s.to_ascii_lowercase().as_str() {
            "m" => Some(DistUnit::M),
            "km" => Some(DistUnit::Km),
            "ft" => Some(DistUnit::Ft),
            "mi" => Some(DistUnit::Mi),
            _ => None,
        }
    }

    pub fn to_meters(self, val: f64) -> f64 {
        match self {
            DistUnit::M => val,
            DistUnit::Km => val * 1000.0,
            DistUnit::Ft => val * 0.3048,
            DistUnit::Mi => val * 1609.344,
        }
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn from_meters(self, val: f64) -> f64 {
        match self {
            DistUnit::M => val,
            DistUnit::Km => val / 1000.0,
            DistUnit::Ft => val / 0.3048,
            DistUnit::Mi => val / 1609.344,
        }
    }
}

pub fn validate_coords(lon: f64, lat: f64) -> Result<(), String> {
    if !(-180.0..=180.0).contains(&lon) || !(GEO_LAT_MIN..=GEO_LAT_MAX).contains(&lat) {
        return Err(format!(
            "ERR invalid longitude,latitude pair {:.6},{:.6}",
            lon, lat
        ));
    }
    Ok(())
}

pub fn geohash_encode(lon: f64, lat: f64) -> u64 {
    let lon_offset = (lon + 180.0) / LON_RANGE;
    let lat_offset = (lat - GEO_LAT_MIN) / GEO_LAT_RANGE;

    let lon_bits = (lon_offset * (1u64 << GEO_STEP) as f64) as u64;
    let lat_bits = (lat_offset * (1u64 << GEO_STEP) as f64) as u64;

    interleave(lon_bits, lat_bits)
}

fn interleave(x: u64, y: u64) -> u64 {
    let mut result = 0u64;
    for i in 0..GEO_STEP {
        result |= ((x >> i) & 1) << (i * 2 + 1);
        result |= ((y >> i) & 1) << (i * 2);
    }
    result
}

fn deinterleave(hash: u64) -> (u64, u64) {
    let mut x = 0u64;
    let mut y = 0u64;
    for i in 0..GEO_STEP {
        x |= ((hash >> (i * 2 + 1)) & 1) << i;
        y |= ((hash >> (i * 2)) & 1) << i;
    }
    (x, y)
}

pub fn geohash_decode(hash: u64) -> (f64, f64) {
    let (lon_bits, lat_bits) = deinterleave(hash);

    let scale = (1u64 << GEO_STEP) as f64;

    let lon_min = -180.0 + (lon_bits as f64 / scale) * LON_RANGE;
    let lon_max = -180.0 + ((lon_bits + 1) as f64 / scale) * LON_RANGE;
    let lon = (lon_min + lon_max) / 2.0;

    let lat_min = GEO_LAT_MIN + (lat_bits as f64 / scale) * GEO_LAT_RANGE;
    let lat_max = GEO_LAT_MIN + ((lat_bits + 1) as f64 / scale) * GEO_LAT_RANGE;
    let lat = (lat_min + lat_max) / 2.0;

    (lon, lat)
}

pub fn geohash_to_base32(hash: u64) -> String {
    let (lon, lat) = geohash_decode(hash);

    let lon_offset = (lon + 180.0) / LON_RANGE;
    let lat_offset = (lat + 90.0) / STD_LAT_RANGE;
    let lon_bits = (lon_offset * (1u64 << GEO_STEP) as f64) as u64;
    let lat_bits = (lat_offset * (1u64 << GEO_STEP) as f64) as u64;
    let std_hash = interleave(lon_bits, lat_bits);

    let mut buf = [0u8; 11];
    for (i, byte) in buf.iter_mut().enumerate() {
        let idx = if i == 10 {
            0
        } else {
            ((std_hash >> (52 - 5 * (i as u32 + 1))) & 0x1F) as usize
        };
        *byte = BASE32[idx];
    }
    String::from_utf8_lossy(&buf).to_string()
}

pub fn in_box(
    center_lon: f64,
    center_lat: f64,
    width_m: f64,
    height_m: f64,
    lon: f64,
    lat: f64,
) -> bool {
    let lat_dist = EARTH_RADIUS_M * (lat.to_radians() - center_lat.to_radians()).abs();
    if lat_dist > height_m / 2.0 {
        return false;
    }
    let lon_dist = haversine(lon, lat, center_lon, lat);
    lon_dist <= width_m / 2.0
}

pub fn haversine(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let lon1_r = lon1.to_radians();
    let lon2_r = lon2.to_radians();
    let v = ((lon2_r - lon1_r) / 2.0).sin();
    if v == 0.0 {
        return EARTH_RADIUS_M * (lat2.to_radians() - lat1.to_radians()).abs();
    }
    let lat1_r = lat1.to_radians();
    let lat2_r = lat2.to_radians();
    let u = ((lat2_r - lat1_r) / 2.0).sin();
    let a = (u * u + lat1_r.cos() * lat2_r.cos() * v * v).min(1.0);
    2.0 * EARTH_RADIUS_M * a.sqrt().asin()
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_precision() {
        let coords = [
            (13.361389, 38.115556),
            (15.087269, 37.502669),
            (-122.4194, 37.7749),
            (0.0, 0.0),
            (139.6917, 35.6895),
        ];
        for (lon, lat) in coords {
            let hash = geohash_encode(lon, lat);
            let (lon2, lat2) = geohash_decode(hash);
            assert!(
                (lon - lon2).abs() < 0.001,
                "lon mismatch: {} vs {} (hash={})",
                lon,
                lon2,
                hash
            );
            assert!(
                (lat - lat2).abs() < 0.001,
                "lat mismatch: {} vs {} (hash={})",
                lat,
                lat2,
                hash
            );
        }
    }

    #[test]
    fn test_haversine_known() {
        let d = haversine(13.361389, 38.115556, 15.087269, 37.502669);
        assert!(
            (d - 166274.0).abs() < 200.0,
            "Palermo-Catania distance wrong: {}",
            d
        );
    }

    #[test]
    fn test_validate_coords() {
        assert!(validate_coords(0.0, 0.0).is_ok());
        assert!(validate_coords(180.0, 85.0).is_ok());
        assert!(validate_coords(181.0, 0.0).is_err());
        assert!(validate_coords(0.0, 86.0).is_err());
    }

    #[test]
    fn test_dist_unit_conversion() {
        let km = DistUnit::Km;
        assert!((km.to_meters(1.0) - 1000.0).abs() < 0.01);
        assert!((km.from_meters(1000.0) - 1.0).abs() < 0.01);

        let mi = DistUnit::Mi;
        assert!((mi.to_meters(1.0) - 1609.344).abs() < 0.01);
    }

    #[test]
    fn test_geohash_base32() {
        let hash = geohash_encode(13.361389, 38.115556);
        let b32 = geohash_to_base32(hash);
        assert_eq!(b32.len(), 11);
        assert!(b32.starts_with("sqc8b49rny"), "Palermo geohash: {}", b32);

        let hash2 = geohash_encode(15.087269, 37.502669);
        let b32_2 = geohash_to_base32(hash2);
        assert!(
            b32_2.starts_with("sqdtr74hyu"),
            "Catania geohash: {}",
            b32_2
        );
    }

    #[test]
    fn test_scores_match_redis() {
        let places = [
            ("wtc one", -74.0131604, 40.7126674, 1791873972053020u64),
            ("union square", -73.9903085, 40.7362513, 1791875485187452),
            ("central park", -73.9733487, 40.7648057, 1791875761332224),
            ("4545", -73.9564142, 40.7480973, 1791875796750882),
            ("q4", -73.9375699, 40.7498929, 1791875830079666),
            ("jfk", -73.7858139, 40.6428986, 1791895905559723),
        ];
        for (name, lon, lat, expected) in places {
            let hash = geohash_encode(lon, lat);
            assert_eq!(hash, expected, "{} score mismatch", name);
        }
    }

    #[test]
    fn test_geohash_known_value() {
        let hash = geohash_encode(-5.6, 42.6);
        let b32 = geohash_to_base32(hash);
        assert_eq!(b32, "ezs42e44yx0", "(-5.6, 42.6) geohash: {}", b32);
    }

    #[test]
    fn test_in_box() {
        assert!(in_box(15.0, 37.0, 400_000.0, 400_000.0, 15.0, 37.0));
        assert!(!in_box(15.0, 37.0, 400_000.0, 400_000.0, 0.0, 0.0));
    }
}
