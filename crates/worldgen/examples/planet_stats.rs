//! Climate statistics of a realistic planet, for calibrating the climate
//! model against the real Earth: mean land rainfall, aridity and
//! temperature per latitude band, and the share of land in each aridity
//! class (UNEP: hyper-arid < 0.05, arid < 0.2, semi-arid < 0.5, dry
//! sub-humid < 0.65, humid above). Real Earth, without Antarctica and
//! Greenland: 7.5%, 12.1%, 17.7%, 9.9%, 52.8%.
//!
//! cargo run --release -p worldgen --example planet_stats -- [seed] [continents|pangea|islands]

use worldgen::realistic::{climate, tectonics};
use worldgen::settings::Planet;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seed: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(42);
    let planet = match args.get(2).map(|s| s.as_str()) {
        Some("pangea") => Planet::Pangea,
        Some("islands") => Planet::Islands,
        _ => Planet::Continents,
    };
    let (w, h) = (768, 768);
    let km = 40000.0 / w as f64;
    let t = tectonics::build(seed, planet, w, h, km);
    let c = climate::simulate(&t.elevation, km, seed, 6.0);
    let mut classes = [0.0f64; 5];
    for j in 0..h {
        let lat = climate::latitude(j, h);
        let mut land = (0.0, 0.0, 0.0, 0);
        let mut sea = (0.0, 0);
        let area = lat.to_radians().cos();
        for i in 0..w {
            let k = i + j * w;
            if t.elevation.data[k] >= 0.0 {
                let ai = c.aridity.data[k] as f64;
                land.0 += c.precipitation.data[k] as f64;
                land.1 += ai;
                land.2 += c.temperature.data[k] as f64;
                land.3 += 1;
                if lat.abs() < 60.0 {
                    let class = [0.05, 0.2, 0.5, 0.65].iter().take_while(|&&b| ai >= b).count();
                    classes[class] += area;
                }
            } else {
                sea.0 += c.precipitation.data[k] as f64;
                sea.1 += 1;
            }
        }
        if j % 48 == 24 {
            let n = land.3.max(1) as f64;
            println!(
                "lat {lat:6.1}  land {:3}  P {:6.0}  AI {:5.2}  T {:5.1}   sea P {:6.0}",
                land.3,
                land.0 / n,
                land.1 / n,
                land.2 / n,
                sea.0 / sea.1.max(1) as f64
            );
        }
    }
    let total: f64 = classes.iter().sum();
    let names = ["hyper-arid", "arid", "semi-arid", "dry sub-humid", "humid"];
    for (name, v) in names.iter().zip(classes) {
        println!("{name:>14}: {:5.1}%", 100.0 * v / total);
    }
}
