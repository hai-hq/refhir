//! The `near` search parameter: reading a query, measuring distance, and the index that lets a
//! search find the places near a point without reading every place.
//!
//! The index is a Z-order curve. A position's latitude and longitude are each cut to 32 bits
//! (about 5 mm of latitude) and their bits are interleaved into one 64-bit number, so places that
//! are close on the map are mostly close in the number. That number, big-endian, is the index key,
//! and the index is ordinary byte-ordered keys, which both storage backends hold and range-scan the
//! way they do for numbers and dates. A search turns the square around its point into a handful of
//! ranges of that number (`Near::cells`) and reads those. A range holds some places outside the
//! circle, so the distance is still checked for each place found, with the same arithmetic that
//! decided every `near` search before the index: the index only decides what is read, never what
//! matches.

use refhir_types::Interval;
use serde_json::Value;

/// Mean radius of the Earth, which the distance between two positions is measured with.
const EARTH_RADIUS_KM: f64 = 6371.0;
const KM_PER_MILE: f64 = 1.60934;
/// How far `near` reaches when the query names no distance. FHIR leaves that to the server.
const DEFAULT_DISTANCE: f64 = 10.0;
/// UCUM, which the units of a `valueDistance` are written in.
const UCUM: &str = "http://unitsofmeasure.org";

/// Most ranges one square is cut into. Each is a read of the index, and cutting finer saves reading
/// places the distance check would only turn away: past a few dozen, a range costs more than the
/// places it saves.
const MAX_PIECES: usize = 40;

/// A circle wider than this (a quarter of the way round the Earth) holds so much of the world that
/// an index finds almost nothing the table does not.
const MAX_ANGLE: f64 = std::f64::consts::FRAC_PI_2;

/// A search within this distance is narrow: cheaper to read through the index than through any
/// other parameter a search might be able to use.
const NARROW_KM: f64 = 200.0;

/// The first byte of a key of a place the index can place: its latitude is within 90 degrees and
/// its longitude within 180. `UNPLACED` is the first byte of a key of one it cannot, which every
/// search reads, so that a position that makes no sense is checked as it always was.
const PLACED: u8 = 0;
const UNPLACED: u8 = 1;

/// A `near` query: where, and how far.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Near {
    latitude: f64,
    longitude: f64,
    limit_km: f64,
    miles: bool,
}

/// The index ranges a `near` search reads.
pub(crate) struct Cells {
    pub ranges: Vec<Interval>,
    /// Whether the circle is small enough for the index to beat reading by another parameter.
    pub narrow: bool,
}

impl Near {
    /// `latitude|longitude`, then optionally `|distance` and `|units`. A distance that is missing
    /// or cannot be read is 10, and only `mi` is read as miles, anything else as kilometres: FHIR
    /// says to assume kilometres when the units are left out.
    pub fn parse(query: &str) -> Option<Self> {
        let mut parts = query.split('|');
        let latitude = parts.next()?.parse::<f64>().ok()?;
        let longitude = parts.next()?.parse::<f64>().ok()?;
        let distance = parts
            .next()
            .and_then(|text| text.parse::<f64>().ok())
            .unwrap_or(DEFAULT_DISTANCE);
        let miles = parts.next() == Some("mi");
        let limit_km = if miles {
            distance * KM_PER_MILE
        } else {
            distance
        };
        Some(Self {
            latitude,
            longitude,
            limit_km,
            miles,
        })
    }

    /// Whether the place `resource` holds is within the distance.
    pub fn matches(&self, resource: &Value) -> bool {
        self.distance_km(resource)
            .is_some_and(|distance| distance <= self.limit_km)
    }

    /// How far the place `resource` holds is from the point, in kilometres.
    pub fn distance_km(&self, resource: &Value) -> Option<f64> {
        let (latitude, longitude) = position(resource)?;
        Some(haversine_km(
            latitude,
            longitude,
            self.latitude,
            self.longitude,
        ))
    }

    /// `distance_km` as the `valueDistance` of an entry's `location-distance` extension, in the
    /// units the query used.
    pub fn value_distance(&self, kilometres: f64) -> Value {
        let (value, unit, code) = if self.miles {
            (kilometres / KM_PER_MILE, "mi", "[mi_i]")
        } else {
            (kilometres, "km", "km")
        };
        // Micrometres of a kilometre are well past what a position says.
        let value = (value * 1e6).round() / 1e6;
        serde_json::json!({"value": value, "unit": unit, "system": UCUM, "code": code})
    }

    /// The ranges of the index that hold every place within the distance, or `None` when the
    /// index would not help: the point or the distance makes no sense, or the circle is so wide that
    /// it holds most of the world.
    pub fn cells(&self) -> Option<Cells> {
        let on_the_map =
            (-90.0..=90.0).contains(&self.latitude) && (-180.0..=180.0).contains(&self.longitude);
        // Not a number is neither, and so is not a distance that reaches anything.
        let reaches = self.limit_km >= 0.0 && self.limit_km.is_finite();
        if !on_the_map || !reaches {
            return None;
        }
        let radius = self.limit_km / EARTH_RADIUS_KM;
        if radius >= MAX_ANGLE {
            return None;
        }
        // The distance is worked out in floating point, which can put a place a few ulps either
        // side of where the geometry does. Reading a little more costs nothing; reading too little
        // loses a match.
        let radius = radius * (1.0 + 1e-9) + 1e-12;
        let mut pieces = Vec::new();
        for square in squares(self.latitude, self.longitude, radius) {
            pieces.extend(cut(&square));
        }
        let mut ranges: Vec<Interval> = merge(pieces).into_iter().map(interval).collect();
        // A position that makes no sense is checked every time, as it always was.
        ranges.push(Interval::prefix(vec![UNPLACED]));
        Some(Cells {
            ranges,
            narrow: self.limit_km <= NARROW_KM,
        })
    }
}

/// The index key of the place `resource` holds, or `None` when it holds none (a resource that
/// `near` could never match). Positions the index cannot place are kept apart: see `UNPLACED`.
pub(crate) fn key(resource: &Value, id: &str) -> Option<Vec<u8>> {
    let (latitude, longitude) = position(resource)?;
    let mut key = Vec::with_capacity(10 + id.len());
    match z_value(latitude, longitude) {
        Some(z) => {
            key.push(PLACED);
            key.extend_from_slice(&z.to_be_bytes());
        }
        None => key.push(UNPLACED),
    }
    key.push(0);
    key.extend_from_slice(id.as_bytes());
    Some(key)
}

/// `Location.position`'s latitude and longitude, as degrees.
pub(crate) fn position(resource: &Value) -> Option<(f64, f64)> {
    let position = resource.get("position")?;
    Some((
        position.get("latitude")?.as_f64()?,
        position.get("longitude")?.as_f64()?,
    ))
}

/// The great-circle distance between two positions, in kilometres.
pub(crate) fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().asin()
}

/// The place on the curve of a position, or `None` for one off the map.
fn z_value(latitude: f64, longitude: f64) -> Option<u64> {
    if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
        return None;
    }
    Some(interleave(
        grid(longitude, -180.0, 360.0),
        grid(latitude, -90.0, 180.0),
    ))
}

/// Which of the 2^32 slices of `span` degrees, from `min`, a coordinate is in. Never decreases as
/// the coordinate grows, which is what lets the cover of a square hold every position in it.
fn grid(degrees: f64, min: f64, span: f64) -> u32 {
    let slice = ((degrees - min) / span * 4_294_967_296.0).floor();
    slice.clamp(0.0, f64::from(u32::MAX)) as u32
}

/// The bits of `x` and `y` taken in turn, `x` first: the Z-order of a point of the grid.
fn interleave(x: u32, y: u32) -> u64 {
    fn spread(value: u32) -> u64 {
        let mut bits = u64::from(value);
        bits = (bits | (bits << 16)) & 0x0000_ffff_0000_ffff;
        bits = (bits | (bits << 8)) & 0x00ff_00ff_00ff_00ff;
        bits = (bits | (bits << 4)) & 0x0f0f_0f0f_0f0f_0f0f;
        bits = (bits | (bits << 2)) & 0x3333_3333_3333_3333;
        (bits | (bits << 1)) & 0x5555_5555_5555_5555
    }
    spread(x) | (spread(y) << 1)
}

/// A rectangle of the grid, edges included.
struct Square {
    x0: u64,
    x1: u64,
    y0: u64,
    y1: u64,
}

/// The squares of the grid that cover the circle of angular `radius` (radians) round a point: one,
/// or two when the circle crosses the antimeridian. A circle that reaches a pole is as wide as the
/// world.
fn squares(latitude: f64, longitude: f64, radius: f64) -> Vec<Square> {
    use std::f64::consts::FRAC_PI_2;
    let centre = latitude.to_radians();
    let (south, north) = (
        (centre - radius).to_degrees(),
        (centre + radius).to_degrees(),
    );
    let rows = (
        grid(south.max(-90.0), -90.0, 180.0),
        grid(north.min(90.0), -90.0, 180.0),
    );
    // How far east and west the circle reaches, in degrees of longitude.
    let reach = if centre + radius >= FRAC_PI_2 || centre - radius <= -FRAC_PI_2 {
        180.0
    } else {
        (radius.sin() / centre.cos()).min(1.0).asin().to_degrees()
    };
    let (west, east) = (longitude - reach, longitude + reach);
    let columns = |from: f64, to: f64| {
        (
            grid(from.max(-180.0), -180.0, 360.0),
            grid(to.min(180.0), -180.0, 360.0),
        )
    };
    let square = |(x0, x1): (u32, u32)| Square {
        x0: u64::from(x0),
        x1: u64::from(x1),
        y0: u64::from(rows.0),
        y1: u64::from(rows.1),
    };
    if reach >= 180.0 {
        vec![square(columns(-180.0, 180.0))]
    } else if west < -180.0 {
        vec![
            square(columns(west + 360.0, 180.0)),
            square(columns(-180.0, east)),
        ]
    } else if east > 180.0 {
        vec![
            square(columns(west, 180.0)),
            square(columns(-180.0, east - 360.0)),
        ]
    } else {
        vec![square(columns(west, east))]
    }
}

/// A square of the grid that is a whole quarter-tree cell: the world halved `level` times in each
/// direction, and which of those cells it is.
#[derive(Clone, Copy)]
struct Cell {
    level: u32,
    x: u64,
    y: u64,
}

enum Overlap {
    Outside,
    Partly,
    Inside,
}

impl Cell {
    fn side(self) -> u64 {
        1 << (32 - self.level)
    }

    fn children(self) -> [Cell; 4] {
        let (level, x, y) = (self.level + 1, self.x * 2, self.y * 2);
        [
            Cell { level, x, y },
            Cell { level, x: x + 1, y },
            Cell { level, x, y: y + 1 },
            Cell {
                level,
                x: x + 1,
                y: y + 1,
            },
        ]
    }

    /// The run of the curve that the cell holds: a cell's places are one run of Z values.
    fn run(self) -> (u64, u64) {
        let side = self.side();
        let first = interleave((self.x * side) as u32, (self.y * side) as u32);
        let bits = 2 * (32 - self.level);
        let last = if bits >= 64 {
            u64::MAX
        } else {
            first | ((1 << bits) - 1)
        };
        (first, last)
    }

    fn overlap(self, square: &Square) -> Overlap {
        let side = self.side();
        let (x0, y0) = (self.x * side, self.y * side);
        let (x1, y1) = (x0 + side - 1, y0 + side - 1);
        if x1 < square.x0 || x0 > square.x1 || y1 < square.y0 || y0 > square.y1 {
            Overlap::Outside
        } else if x0 >= square.x0 && x1 <= square.x1 && y0 >= square.y0 && y1 <= square.y1 {
            Overlap::Inside
        } else {
            Overlap::Partly
        }
    }
}

/// Runs of the curve that hold every position of `square`, and as few others as at most
/// `MAX_PIECES` runs allow.
///
/// The world is halved again and again, and a cell inside the square is kept whole while one that
/// is cut by an edge is halved further, until another halving would make more runs than
/// `MAX_PIECES` (neighbouring cells on the curve are one run). Cells still cut by an edge then
/// stand for everything they hold, which is more than the square, and never less.
fn cut(square: &Square) -> Vec<(u64, u64)> {
    let root = Cell {
        level: 0,
        x: 0,
        y: 0,
    };
    match root.overlap(square) {
        Overlap::Outside => return Vec::new(),
        Overlap::Inside => return vec![root.run()],
        Overlap::Partly => {}
    }
    let mut kept: Vec<(u64, u64)> = Vec::new();
    let mut open = vec![root];
    for _ in 0..32 {
        let mut inside = Vec::new();
        let mut partly = Vec::new();
        for cell in &open {
            for child in cell.children() {
                match child.overlap(square) {
                    Overlap::Outside => {}
                    Overlap::Inside => inside.push(child.run()),
                    Overlap::Partly => partly.push(child),
                }
            }
        }
        let whole: Vec<(u64, u64)> = kept
            .iter()
            .chain(&inside)
            .copied()
            .chain(partly.iter().map(|cell| cell.run()))
            .collect();
        if merge(whole).len() > MAX_PIECES {
            break;
        }
        kept.extend(inside);
        open = partly;
        if open.is_empty() {
            break;
        }
    }
    kept.extend(open.iter().map(|cell| cell.run()));
    kept
}

/// The runs in order, with those that touch or overlap made one.
fn merge(mut runs: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    runs.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(runs.len());
    for (first, last) in runs {
        match merged.last_mut() {
            Some((_, end)) if *end == u64::MAX || first <= *end + 1 => *end = (*end).max(last),
            _ => merged.push((first, last)),
        }
    }
    merged
}

/// The range of keys of the places on a run of the curve.
fn interval((first, last): (u64, u64)) -> Interval {
    let mut start = vec![PLACED];
    start.extend_from_slice(&first.to_be_bytes());
    let end = match last.checked_add(1) {
        Some(next) => {
            let mut end = vec![PLACED];
            end.extend_from_slice(&next.to_be_bytes());
            end
        }
        // The last run ends where the keys of the places that cannot be placed begin.
        None => vec![UNPLACED],
    };
    Interval {
        start,
        end: Some(end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A small, fixed stream of numbers in [0, 1).
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }

        fn between(&mut self, low: f64, high: f64) -> f64 {
            low + self.next() * (high - low)
        }
    }

    fn place(latitude: f64, longitude: f64) -> Value {
        json!({"resourceType": "Location", "position": {"latitude": latitude, "longitude": longitude}})
    }

    fn read(ranges: &[Interval], key: &[u8]) -> bool {
        ranges.iter().any(|range| range.contains(key))
    }

    #[test]
    fn the_curve_orders_a_cell_as_one_run() {
        // Whatever the level, the places of a cell are one run, and the cells of a level tile the
        // curve: no place is in two cells, and none is in none.
        for level in [1u32, 2, 3, 5] {
            let side = 1u64 << (32 - level);
            let cells = 1u64 << level;
            let mut runs: Vec<(u64, u64)> = (0..cells)
                .flat_map(|x| (0..cells).map(move |y| Cell { level, x, y }.run()))
                .collect();
            runs.sort_unstable();
            let mut next = 0u64;
            for (first, last) in &runs {
                assert_eq!(*first, next, "level {level}");
                assert_eq!(last - first + 1, side * side, "level {level}");
                next = last.wrapping_add(1);
            }
            assert_eq!(
                next, 0,
                "the runs of level {level} end at the end of the curve"
            );
        }
    }

    #[test]
    fn a_place_is_found_in_the_ranges_of_every_circle_that_holds_it() {
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        let mut searches = 0;
        let mut narrowed = 0;
        for round in 0..3000 {
            // Centres anywhere, with a good share near the poles and the antimeridian, where the
            // square round a circle bends.
            let latitude = match round % 4 {
                0 => random.between(-90.0, 90.0),
                1 => random.between(80.0, 90.0),
                2 => random.between(-90.0, -80.0),
                _ => random.between(-40.0, 40.0),
            };
            let longitude = match round % 3 {
                0 => random.between(-180.0, 180.0),
                1 => random.between(175.0, 180.0),
                _ => random.between(-180.0, -175.0),
            };
            let kilometres = match round % 5 {
                0 => 0.0,
                1 => random.between(0.0, 1.0),
                2 => random.between(0.0, 30.0),
                3 => random.between(0.0, 400.0),
                _ => random.between(0.0, 3000.0),
            };
            let near = Near::parse(&format!("{latitude}|{longitude}|{kilometres}|km")).unwrap();
            let Some(cells) = near.cells() else {
                continue;
            };
            searches += 1;
            let reach = kilometres / 111.0 + 0.01;
            let wide = reach / latitude.to_radians().cos().abs().max(0.01);
            for n in 0..300 {
                // Places round the circle, and some exactly at its centre.
                let (la, lo) = if n % 7 == 0 {
                    (latitude, longitude)
                } else {
                    let lo = longitude + random.between(-wide, wide);
                    (
                        (latitude + random.between(-reach, reach)).clamp(-90.0, 90.0),
                        // Past the antimeridian is the other side of the map.
                        if lo > 180.0 {
                            lo - 360.0
                        } else if lo < -180.0 {
                            lo + 360.0
                        } else {
                            lo
                        },
                    )
                };
                let resource = place(la, lo);
                if near.matches(&resource) {
                    assert!(
                        read(&cells.ranges, &key(&resource, "x").unwrap()),
                        "({la}, {lo}) is within {kilometres} km of ({latitude}, {longitude}) \
                         and was not read"
                    );
                }
            }
            if cells.ranges.len() <= 2 * MAX_PIECES + 1 {
                narrowed += 1;
            }
        }
        assert!(searches > 2500, "{searches}");
        assert_eq!(
            narrowed, searches,
            "a search reads a few dozen ranges at most"
        );
    }

    #[test]
    fn a_place_exactly_at_the_distance_is_read() {
        // The distance of a place is what the search measures with, taken as the radius: not a
        // hair inside it, and not a hair outside.
        let mut random = Random(0x2545_f491_4f6c_dd1d);
        for _ in 0..2000 {
            let (latitude, longitude) =
                (random.between(-85.0, 85.0), random.between(-180.0, 180.0));
            let (la, lo) = (
                latitude + random.between(-2.0, 2.0),
                (longitude + random.between(-2.0, 2.0)).clamp(-180.0, 180.0),
            );
            let exact = haversine_km(la, lo, latitude, longitude);
            let resource = place(la, lo);
            let stored = key(&resource, "x").unwrap();
            for (radius, wanted) in [
                (exact, true),
                (exact.next_up(), true),
                (exact.next_down(), false),
            ] {
                let near = Near::parse(&format!("{latitude}|{longitude}|{radius}")).unwrap();
                assert_eq!(near.matches(&resource), wanted, "{exact} {radius}");
                if wanted {
                    let cells = near.cells().unwrap();
                    assert!(read(&cells.ranges, &stored), "{exact} {radius}");
                }
            }
        }
    }

    #[test]
    fn a_place_the_index_cannot_place_is_always_read() {
        for (latitude, longitude) in [(91.0, 10.0), (-90.5, 10.0), (10.0, 181.0), (10.0, -200.0)] {
            let stored = key(&place(latitude, longitude), "x").unwrap();
            assert_eq!(stored[0], UNPLACED);
            let cells = Near::parse("6.5|3.4|10|km").unwrap().cells().unwrap();
            assert!(read(&cells.ranges, &stored));
        }
        // And a resource with no position has no key at all.
        assert_eq!(key(&json!({"resourceType": "Location"}), "x"), None);
        assert_eq!(
            key(
                &json!({"position": {"latitude": "6.5", "longitude": 3.4}}),
                "x"
            ),
            None
        );
    }

    #[test]
    fn a_query_the_index_cannot_use_reads_everything() {
        for query in [
            "95|10|10|km",
            "10|190|10|km",
            "10|10|-5|km",
            "10|10|16000|km",
            "10|10|10000|mi",
        ] {
            assert!(Near::parse(query).unwrap().cells().is_none(), "{query}");
        }
        assert_eq!(Near::parse("10"), None);
        assert_eq!(Near::parse("10|x"), None);
        assert_eq!(Near::parse("a|10"), None);
        assert!(Near::parse("10|10").unwrap().cells().is_some());
        // The antipode is as far as any place is, and nothing about it is special to the index.
        assert!(Near::parse("0|0|100|km").unwrap().cells().is_some());
    }

    #[test]
    fn the_distance_is_in_kilometres_unless_miles_are_asked_for() {
        let near = |query: &str| Near::parse(query).unwrap();
        // A place `km` kilometres east of (6.5, 3.4), near enough: along a parallel a degree of
        // longitude is a fixed length.
        let at = |km: f64| {
            place(
                6.5,
                3.4 + (km / (EARTH_RADIUS_KM * 6.5f64.to_radians().cos())).to_degrees(),
            )
        };
        // Units left out are kilometres, as FHIR says, and so is a distance with none named: ten.
        for (query, nine_nine, ten_one) in [
            ("6.5|3.4", true, false),
            ("6.5|3.4|10", true, false),
            ("6.5|3.4|10|km", true, false),
            ("6.5|3.4||km", true, false),
            ("6.5|3.4|9.5", false, false),
            ("6.5|3.4|9.5|km", false, false),
            ("6.5|3.4|9|km", false, false),
            ("6.5|3.4|10.5|km", true, true),
        ] {
            assert_eq!(near(query).matches(&at(9.9)), nine_nine, "{query}");
            assert_eq!(near(query).matches(&at(10.1)), ten_one, "{query}");
        }
        // Six miles are 9.66 kilometres, and a mile is not a kilometre.
        assert!(near("6.5|3.4|6.2|mi").matches(&at(9.9)));
        assert!(!near("6.5|3.4|6.2|mi").matches(&at(10.1)));
        assert!(!near("6.5|3.4|6|mi").matches(&at(9.9)));
        assert!(near("6.5|3.4|6.3|mi").matches(&at(10.1)));
        assert!(near("6.5|3.4|6|km").matches(&place(6.5, 3.4)));
    }
}
