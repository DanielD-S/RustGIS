//! Map projections: how the flat map lays the Earth out (the globe is in [`crate::globe`]).
//!
//! A map is drawn in Web Mercator by default, or in any projected coordinate system: a preset
//! (world and polar projections, regional equal-area grids) or an EPSG code. The camera then
//! works in that system's units, so panning, zooming and hit testing are the same for all of
//! them; only conversions between longitude/latitude and map coordinates go through here.

use rustgis_geom::{Coord, Extent, lonlat_to_merc, merc_to_lonlat};

/// A named projection: (id, label, PROJ definition).
pub const PRESETS: &[(&str, &str, &str)] = &[
    ("equirectangular", "Equirectangular (Plate Carrée)", "+proj=eqc +lat_ts=0 +lon_0=0 +datum=WGS84 +units=m +no_defs"),
    ("miller", "Miller Cylindrical", "+proj=mill +lon_0=0 +R_A +datum=WGS84 +units=m +no_defs"),
    ("mollweide", "Mollweide (equal-area world)", "+proj=moll +lon_0=0 +datum=WGS84 +units=m +no_defs"),
    ("cylindricalEqualArea", "Lambert Cylindrical Equal Area", "+proj=cea +lon_0=0 +lat_ts=0 +datum=WGS84 +units=m +no_defs"),
    ("robinson", "Robinson", "+proj=robin +lon_0=0 +datum=WGS84 +units=m +no_defs"),
    ("equalEarth", "Equal Earth", "+proj=eqearth +lon_0=0 +datum=WGS84 +units=m +no_defs"),
    ("naturalEarth", "Natural Earth", "+proj=natearth +lon_0=0 +datum=WGS84 +units=m +no_defs"),
    ("winkelTripel", "Winkel Tripel", "+proj=wintri +lon_0=0 +datum=WGS84 +units=m +no_defs"),
    ("eckertIV", "Eckert IV (equal-area world)", "+proj=eck4 +lon_0=0 +datum=WGS84 +units=m +no_defs"),
    ("northPolar", "North Polar Stereographic", "+proj=stere +lat_0=90 +lat_ts=70 +lon_0=0 +datum=WGS84 +units=m +no_defs"),
    ("southPolar", "South Polar Stereographic", "+proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 +datum=WGS84 +units=m +no_defs"),
    (
        "europeEqualArea",
        "Europe Equal Area (ETRS89 LAEA)",
        "+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +ellps=GRS80 +units=m +no_defs",
    ),
    (
        "usaAlbers",
        "USA Contiguous Albers Equal Area",
        "+proj=aea +lat_0=23 +lon_0=-96 +lat_1=29.5 +lat_2=45.5 +datum=NAD83 +units=m +no_defs",
    ),
];

/// Whether local views (`local:lon,lat`, azimuthal equidistant) can be drawn in this build. They
/// need proj4rs' `aeqd`, which the browser build leaves out (Cargo.toml); asking the projection
/// itself, rather than the target, keeps the command, the ribbon and the tests in step with it.
pub fn local_views_supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| Projector::new("local:0,0").is_ok())
}

/// The PROJ definition and label of a projection code: a preset id or `EPSG:<code>`.
pub fn lookup(code: &str) -> Result<(String, String), String> {
    let code = code.trim();
    // `local:lon,lat`: an azimuthal equidistant projection centred on a point (a local scene).
    if let Some(rest) = code.strip_prefix("local:") {
        // Exactly two finite numbers; anything else is refused rather than half-read.
        let n: Option<Vec<f64>> = rest.split(',').map(|x| x.trim().parse::<f64>().ok().filter(|v| v.is_finite())).collect();
        let Some(&[lon, lat]) = n.as_deref() else { return Err("a local view is local:lon,lat".into()) };
        let (lon, lat) = ((lon + 180.0).rem_euclid(360.0) - 180.0, lat.clamp(-89.0, 89.0));
        return Ok((
            format!("+proj=aeqd +lat_0={lat} +lon_0={lon} +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs"),
            format!("Local ({lon:.2}°, {lat:.2}°)"),
        ));
    }
    if let Some((_, label, def)) = PRESETS.iter().find(|(id, label, _)| id.eq_ignore_ascii_case(code) || label.eq_ignore_ascii_case(code)) {
        return Ok((def.to_string(), label.to_string()));
    }
    // `3035`, `EPSG:3035` or `epsg 3035`: an optional prefix, then only digits.
    let lower = code.to_ascii_lowercase();
    let digits = lower.strip_prefix("epsg").map(|r| r.trim_start_matches([':', ' '])).unwrap_or(&lower);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("unknown projection '{code}'"));
    }
    let unknown = || format!("unknown coordinate system EPSG:{}", digits.trim_start_matches('0'));
    let n = digits.parse::<u16>().map_err(|_| unknown())?;
    let def = rustgis_data::raster::crs::epsg_proj4(n).ok_or_else(unknown)?;
    if def.contains("+proj=longlat") {
        return Err(format!("EPSG:{n} is in degrees: pick a projected coordinate system"));
    }
    if def.contains("+proj=geocent") {
        return Err(format!("EPSG:{n} isn't a map projection"));
    }
    Ok((def.to_string(), format!("EPSG:{n}")))
}

/// Where a projection is meant to be used. Points outside are not drawn: far from it most
/// projections distort wildly, fold back or run off to infinity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Domain {
    /// The whole world (cylindrical and pseudo-cylindrical projections).
    World,
    /// A longitude/latitude box (regional presets).
    Box(Extent),
    /// Within `radius` degrees of a centre (azimuthal and stereographic projections).
    Cap { center: Coord, radius: f64 },
    /// Within `half` degrees of longitude of a central meridian (transverse Mercator, UTM).
    Band { lon0: f64, half: f64 },
}

impl Domain {
    /// The area of use for a regional preset, or one worked out from the PROJ parameters.
    fn of(id: Option<&str>, def: &str) -> Domain {
        match id {
            Some("europeEqualArea") => return Domain::Box(Extent::new(-35.0, 25.0, 50.0, 75.0)),
            Some("usaAlbers") => return Domain::Box(Extent::new(-130.0, 18.0, -60.0, 55.0)),
            _ => {}
        }
        let param = |k: &str| -> Option<f64> {
            def.split_whitespace().find_map(|t| t.strip_prefix(&format!("+{k}="))).and_then(|v| v.parse().ok())
        };
        let proj = def.split_whitespace().find_map(|t| t.strip_prefix("+proj=")).unwrap_or("");
        let (lon0, lat0) = (param("lon_0").unwrap_or(0.0), param("lat_0").unwrap_or(0.0));
        match proj {
            "tmerc" | "etmerc" | "utm" => {
                let lon0 = if proj == "utm" { param("zone").map(|z| z * 6.0 - 183.0).unwrap_or(lon0) } else { lon0 };
                Domain::Band { lon0, half: 30.0 }
            }
            "stere" | "sterea" | "somerc" | "krovak" => Domain::Cap { center: Coord::new(lon0, lat0), radius: 60.0 },
            "laea" | "aeqd" => Domain::Cap { center: Coord::new(lon0, lat0), radius: 120.0 },
            "aea" | "lcc" => {
                let (a, b) = (param("lat_1").unwrap_or(lat0), param("lat_2").unwrap_or(lat0));
                let (lo, hi) = (a.min(b) - 35.0, a.max(b) + 35.0);
                Domain::Box(Extent::new(lon0 - 90.0, lo.max(-89.0), lon0 + 90.0, hi.min(89.0)))
            }
            "geos" => Domain::Cap { center: Coord::new(lon0, 0.0), radius: 75.0 },
            _ => Domain::World,
        }
    }

    pub fn contains(&self, c: Coord) -> bool {
        match *self {
            Domain::World => true,
            // A box centred near the antimeridian (lon_0 = 150) runs past ±180°.
            Domain::Box(e) => [0.0, 360.0, -360.0].iter().any(|d| e.contains(Coord::new(c.x + d, c.y))),
            Domain::Cap { center, radius } => rustgis_geom::haversine_m(center, c) <= radius.to_radians() * rustgis_geom::EARTH_RADIUS_M,
            Domain::Band { lon0, half } => ((c.x - lon0 + 540.0).rem_euclid(360.0) - 180.0).abs() <= half && c.y.abs() <= 84.0,
        }
    }
}

/// How a projection is computed: by the PROJ port, or here (world projections it lacks).
#[derive(Clone)]
enum Engine {
    Proj { native: Box<proj4rs::Proj>, wgs84: Box<proj4rs::Proj> },
    World(crate::world::World),
}

/// Converts between longitude/latitude and the coordinates of a custom projection.
#[derive(Clone)]
pub struct Projector {
    engine: Engine,
    /// Metres per map unit.
    to_meter: f64,
    domain: Domain,
}

impl std::fmt::Debug for Projector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match &self.engine {
            Engine::Proj { native, .. } => native.projname(),
            Engine::World(w) => w.kind.proj_name(),
        };
        write!(f, "Projector({name})")
    }
}

/// Map coordinates beyond this are treated as unprojectable (the projection's edge).
const LIMIT: f64 = 4.0e7;

impl Projector {
    /// The projector for a code `lookup` accepts: a preset id or name, `EPSG:<n>` or
    /// `local:lon,lat`. World projections proj4rs lacks are computed here; the rest go to
    /// proj4rs. Fails when the code is unknown, or when its method can't be computed (its
    /// `+proj=` name is in the message), so a map is never drawn in another projection than the
    /// one it names.
    pub fn new(code: &str) -> Result<Projector, String> {
        let (def, _) = lookup(code)?;
        let code_t = code.trim();
        let preset = PRESETS
            .iter()
            .find(|(id, label, _)| id.eq_ignore_ascii_case(code_t) || label.eq_ignore_ascii_case(code_t))
            .map(|(id, _, _)| *id);
        let domain = Domain::of(preset, &def);
        let proj = def.split_whitespace().find_map(|t| t.strip_prefix("+proj=")).unwrap_or("");
        if let Some(kind) = crate::world::Kind::from_proj_name(proj) {
            let lon0 = def.split_whitespace().find_map(|t| t.strip_prefix("+lon_0=")).and_then(|v| v.parse().ok()).unwrap_or(0.0);
            let lon0 = if f64::is_finite(lon0) { lon0 } else { 0.0 };
            return Ok(Projector { engine: Engine::World(crate::world::World { kind, lon0 }), to_meter: 1.0, domain });
        }
        let native = proj4rs::Proj::from_proj_string(&def).map_err(|e| match e {
            // A known code whose method isn't implemented (EPSG:5880's +proj=poly, say): name it.
            proj4rs::errors::Error::ProjectionNotFound => format!("{code}: its projection method (+proj={proj}) isn't supported"),
            e => format!("{code}: {e}"),
        })?;
        let wgs84 = proj4rs::Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs").map_err(|e| e.to_string())?;
        let to_meter = native.to_meter();
        let engine = Engine::Proj { native: Box::new(native), wgs84: Box::new(wgs84) };
        Ok(Projector { engine, to_meter: if to_meter > 0.0 { to_meter } else { 1.0 }, domain })
    }

    /// Longitude/latitude to map coordinates, `None` outside the projection.
    pub fn forward(&self, c: Coord) -> Option<Coord> {
        if !c.is_finite() {
            return None;
        }
        let (native, wgs84) = match &self.engine {
            Engine::Proj { native, wgs84 } => (native, wgs84),
            Engine::World(w) => return w.forward(c),
        };
        let mut p = (c.x.to_radians(), c.y.clamp(-90.0, 90.0).to_radians(), 0.0);
        proj4rs::transform::transform(wgs84, native, &mut p).ok()?;
        (p.0.is_finite() && p.1.is_finite() && p.0.abs() < LIMIT && p.1.abs() < LIMIT).then(|| Coord::new(p.0, p.1))
    }

    /// Like [`Projector::forward`], but also rejects points that don't map back to themselves
    /// (wrapped or folded parts of a projection, such as the far hemisphere of a polar one).
    pub fn forward_checked(&self, c: Coord) -> Option<Coord> {
        if !self.domain.contains(c) {
            return None;
        }
        let m = self.forward(c)?;
        // World projections don't fold, and on their edges (±180°) the inverse is too imprecise
        // to compare: only projections limited to an area of use need the round trip.
        if self.domain == Domain::World {
            return Some(m);
        }
        let back = self.inverse(m)?;
        let dlon = ((back.x - c.x + 540.0).rem_euclid(360.0) - 180.0).abs();
        // At a pole every longitude is the same point.
        let same = (dlon < 1e-3 && (back.y - c.y).abs() < 1e-3) || (c.y.abs() > 89.99 && (back.y - c.y).abs() < 1e-3);
        same.then_some(m)
    }

    /// Map coordinates to longitude/latitude.
    pub fn inverse(&self, m: Coord) -> Option<Coord> {
        if !m.is_finite() {
            return None;
        }
        let (native, wgs84) = match &self.engine {
            Engine::Proj { native, wgs84 } => (native, wgs84),
            Engine::World(w) => return w.inverse(m),
        };
        let mut p = (m.x, m.y, 0.0);
        proj4rs::transform::transform(native, wgs84, &mut p).ok()?;
        let c = Coord::new(p.0.to_degrees(), p.1.to_degrees());
        (c.is_finite() && c.y.abs() <= 90.0 + 1e-9).then_some(c)
    }

    pub fn to_meter(&self) -> f64 {
        self.to_meter
    }

    /// Ground metres per map unit at a map point, measured: a 1 km step east-west (the scale
    /// bar's direction; north-south on the projection's edge) compared with the great-circle
    /// distance it covers, on the sphere Web Mercator's scale uses so both agree. Falls back to
    /// the unit size where no step stays in the projection.
    pub fn ground_per_unit(&self, at: Coord) -> f64 {
        let Some(c) = self.inverse(at) else { return self.to_meter };
        let step = 1000.0 / self.to_meter;
        let radius_ratio = rustgis_geom::MERCATOR_R / rustgis_geom::EARTH_RADIUS_M;
        let steps = [Coord::new(step, 0.0), Coord::new(-step, 0.0), Coord::new(0.0, step), Coord::new(0.0, -step)];
        steps
            .iter()
            .filter_map(|s| self.inverse(Coord::new(at.x + s.x, at.y + s.y)))
            .map(|d| rustgis_geom::haversine_m(c, d) * radius_ratio / step)
            .find(|k| k.is_finite() && *k > 0.0)
            .unwrap_or(self.to_meter)
    }

    pub fn domain(&self) -> Domain {
        self.domain
    }
}

/// The flat map's projection, ready to convert coordinates.
#[derive(Clone, Debug)]
pub enum Flat {
    /// Web Mercator (also the camera space of the globe).
    Mercator,
    Custom(std::sync::Arc<Projector>),
}

impl Flat {
    /// Longitude/latitude to map coordinates.
    pub fn forward(&self, c: Coord) -> Option<Coord> {
        match self {
            Flat::Mercator => c.is_finite().then(|| lonlat_to_merc(c)),
            Flat::Custom(p) => p.forward(c),
        }
    }

    /// For geometry and other bulk conversion: drops points a projection folds or wraps.
    pub fn forward_checked(&self, c: Coord) -> Option<Coord> {
        match self {
            Flat::Mercator => self.forward(c),
            Flat::Custom(p) => p.forward_checked(c),
        }
    }

    /// Map coordinates to longitude/latitude.
    pub fn inverse(&self, m: Coord) -> Option<Coord> {
        match self {
            Flat::Mercator => m.is_finite().then(|| merc_to_lonlat(m)),
            Flat::Custom(p) => p.inverse(m),
        }
    }

    /// Ground metres per map unit at a map point (Mercator stretches with latitude).
    pub fn ground_per_unit(&self, at: Coord) -> f64 {
        match self {
            Flat::Mercator => merc_to_lonlat(at).y.to_radians().cos().abs().max(0.01),
            Flat::Custom(p) => p.ground_per_unit(at),
        }
    }

    /// The map-coordinate box around a longitude/latitude extent (from points along its edges and
    /// inside it, so curved projections are covered).
    pub fn extent(&self, e: &Extent) -> Extent {
        let mut out = Extent::EMPTY;
        if e.is_empty() {
            return out;
        }
        let n = 16;
        for i in 0..=n {
            for j in 0..=n {
                let c = Coord::new(e.min_x + e.width() * i as f64 / n as f64, e.min_y + e.height() * j as f64 / n as f64);
                if let Some(m) = self.forward_checked(c) {
                    out.expand(m);
                }
            }
        }
        out
    }
}

thread_local! {
    static PROJECTORS: std::cell::RefCell<std::collections::HashMap<String, Option<std::sync::Arc<Projector>>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

impl Flat {
    /// The flat projection behind a map projection (the globe's camera space is Web Mercator; an
    /// unusable custom code falls back to it). Parsed projections are kept per thread.
    pub fn of(p: &crate::Projection) -> Flat {
        let crate::Projection::Custom(code) = p else { return Flat::Mercator };
        let key = code.trim().to_ascii_lowercase();
        PROJECTORS.with(|cell| {
            let mut map = cell.borrow_mut();
            // Codes come from project files and scripts: keep the cache bounded.
            if map.len() >= 32 && !map.contains_key(&key) {
                map.clear();
            }
            let entry = map.entry(key).or_insert_with(|| Projector::new(code).ok().map(std::sync::Arc::new));
            entry.clone().map(Flat::Custom).unwrap_or(Flat::Mercator)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_codes_are_read_strictly() {
        assert!(lookup("local:12.5,41.9").is_ok());
        for bad in ["local:a,10,20", "local:10", "local:10,20,30", "local:10,inf", "local:"] {
            assert!(lookup(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn presets_and_epsg_codes() {
        for (id, _, _) in PRESETS {
            let p = Projector::new(id).unwrap_or_else(|e| panic!("{id}: {e}"));
            let c = Coord::new(10.0, if id.contains("south") { -75.0 } else { 50.0 });
            let m = p.forward(c).unwrap_or_else(|| panic!("{id}"));
            let back = p.inverse(m).unwrap();
            assert!((back.x - c.x).abs() < 1e-6 && (back.y - c.y).abs() < 1e-6, "{id}: {back:?}");
        }
        assert!(Projector::new("EPSG:3035").is_ok());
        assert!(Projector::new("epsg:32633").is_ok());
        assert!(Projector::new("3857").is_ok());
        assert!(Projector::new("EPSG:4326").is_err(), "degrees are not a map projection");
        assert!(Projector::new("EPSG:1").is_err());
        assert!(Projector::new("bogus").is_err());
        assert!(Projector::new("12x").is_err());
        assert!(Projector::new("epsgfoo32617").is_err());
        // A conic centred on the Pacific keeps points across the antimeridian.
        let pacific = Domain::of(None, "+proj=lcc +lat_1=30 +lat_2=60 +lon_0=150");
        assert!(pacific.contains(Coord::new(-170.0, 45.0)) && pacific.contains(Coord::new(120.0, 45.0)));
        assert!(!pacific.contains(Coord::new(0.0, 45.0)));
        assert!(Projector::new("EPSG 32617").is_ok());
        assert!(lookup("EPSG:99999").unwrap_err().contains("coordinate system"), "out of range is an unknown code");
        // Equal Earth by its EPSG code goes to the built-in engine too.
        let ee = Projector::new("EPSG:8857").unwrap();
        assert!(format!("{ee:?}").contains("eqearth"));
        // Codes crs-definitions lacks (EPSG 9020–20003) come from the data crate's own table
        // (opengeos/RustGIS#198): MAGNA-SIRGAS 2018 / Origen-Nacional, SIRGAS-Chile 2016 / UTM 19S.
        let (bogota, santiago) = (Coord::new(-74.08, 4.65), Coord::new(-70.65, -33.45));
        for (ok, proj, city) in [("EPSG:9377", "tmerc", bogota), ("epsg:9155", "utm", santiago)] {
            let (def, label) = lookup(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
            assert!(def.contains(&format!("+proj={proj}")) && label == ok.to_ascii_uppercase(), "{ok}: {def} {label}");
            let p = Projector::new(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
            let back = p.inverse(p.forward(city).unwrap()).unwrap();
            assert!((back.x - city.x).abs() < 1e-7 && (back.y - city.y).abs() < 1e-7, "{ok}: {back:?}");
        }
        assert!(lookup("EPSG:9148").unwrap_err().contains("degrees"), "SIRGAS-Chile 2013 is geographic");
    }

    #[test]
    fn codes_whose_method_cannot_be_drawn_are_refused() {
        // EPSG:5880 (SIRGAS 2000 / Brazil Polyconic) is in the EPSG table, but its method
        // (+proj=poly) has no implementation: refuse it rather than accept it and draw the map
        // in Web Mercator while naming EPSG:5880 (opengeos/RustGIS#208).
        assert!(lookup("EPSG:5880").is_ok(), "the code itself is known");
        let e = Projector::new("EPSG:5880").unwrap_err();
        assert!(e.contains("EPSG:5880") && e.contains("poly"), "{e}");
        let e = crate::Projection::from_code("EPSG:5880").unwrap_err();
        assert!(e.contains("poly"), "{e}");
        // Codes that can be drawn are still accepted, by number or preset.
        for ok in ["EPSG:3035", "epsg:32633", "EPSG:8858", "equalEarth", "usaAlbers", "mercator", "globe"] {
            crate::Projection::from_code(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        // Local views are accepted exactly where they can be drawn (`aeqd` is native-only, Cargo.toml).
        let local = crate::Projection::from_code("local:12.5,41.9");
        assert_eq!(local.is_ok(), local_views_supported(), "{local:?}");
        // Pinned to the target on purpose, so the probe can't drift from the Cargo.toml split unnoticed.
        assert_eq!(local_views_supported(), !cfg!(target_arch = "wasm32"), "aeqd on native targets only");
    }

    #[test]
    fn local_views_are_azimuthal_equidistant() {
        if !local_views_supported() {
            return; // the browser build (see codes_whose_method_cannot_be_drawn_are_refused)
        }
        // map.viewMode "local" relies on +proj=aeqd: it must build (not fall back to Web Mercator)
        // and keep true distances from its centre.
        let p = Projector::new("local:12.5,41.9").unwrap_or_else(|e| panic!("{e}"));
        let o = p.forward(Coord::new(12.5, 41.9)).unwrap();
        assert!(o.x.abs() < 1e-6 && o.y.abs() < 1e-6, "the centre is the origin: {o:?}");
        let c = Coord::new(13.5, 42.4);
        let m = p.forward(c).unwrap();
        let back = p.inverse(m).unwrap();
        assert!((back.x - c.x).abs() < 1e-7 && (back.y - c.y).abs() < 1e-7, "{back:?}");
        // Distance from the centre on the map is the WGS84 geodesic distance (Vincenty: 99 580.4 m).
        let d = (m.x * m.x + m.y * m.y).sqrt();
        assert!((d - 99_580.4).abs() < 1.0, "{d}");
    }

    #[test]
    fn folded_points_are_rejected_and_extents_project() {
        let north = Projector::new("northPolar").unwrap();
        assert!(north.forward_checked(Coord::new(0.0, 60.0)).is_some());
        assert!(north.forward_checked(Coord::new(0.0, -80.0)).is_none(), "the far pole");
        assert!(north.forward_checked(Coord::new(120.0, -5.0)).is_none(), "the southern hemisphere");
        let utm = Projector::new("EPSG:32633").unwrap();
        assert_eq!(utm.domain(), Domain::Band { lon0: 15.0, half: 30.0 });
        assert!(utm.forward_checked(Coord::new(15.0, 50.0)).is_some() && utm.forward_checked(Coord::new(-100.0, 40.0)).is_none());
        let usa = Projector::new("usaAlbers").unwrap();
        assert!(usa.forward_checked(Coord::new(-95.0, 38.0)).is_some() && usa.forward_checked(Coord::new(100.0, 30.0)).is_none());
        let moll = Flat::Custom(std::sync::Arc::new(Projector::new("mollweide").unwrap()));
        let e = moll.extent(&Extent::new(-180.0, -90.0, 180.0, 90.0));
        assert!(e.width() > 3.55e7 && e.height() > 1.75e7, "the whole world, edges included: {e:?}");
        // Mollweide's parallels are true at about ±40.7° and squeezed (0.9003) at the equator:
        // a map unit there covers 1/0.9003 m of ground.
        assert!((moll.ground_per_unit(Coord::new(0.0, 0.0)) - 1.0 / 0.9003).abs() < 2e-3);
        let at = moll.forward(Coord::new(10.0, 40.73)).unwrap();
        assert!((moll.ground_per_unit(at) - 1.0).abs() < 5e-3);
        // A preset by label gets the same area of use as by id.
        assert_eq!(Projector::new("USA Contiguous Albers Equal Area").unwrap().domain(), Projector::new("usaAlbers").unwrap().domain());
        assert!(Flat::Mercator.inverse(Coord::new(f64::NAN, 0.0)).is_none());
    }
}
