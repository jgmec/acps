use anyhow::{bail, Result};
use h3o::{CellIndex, LatLng, Resolution};
use macn::Mac;
use prost::Message;
use reqwest::Client;
use tryhard::retry_fn;

use crate::protobuf::{AppleWLoc, WifiDevice};

pub struct Location {
    pub lat: f64,
    pub lon: f64,
}

impl Location {
    pub fn cell(&self) -> CellIndex {
        let loc = LatLng::new(self.lat, self.lon).expect("invalid location");
        loc.to_cell(Resolution::Ten)
    }

    pub fn celln(&self) -> u64 {
        self.cell().into()
    }
}

impl Location {
    fn from_raw(value: crate::protobuf::Location) -> Option<Self> {
        let (lat, lon) = (value.latitude, value.longitude);
        let (lat, lon) = (coord(lat), coord(lon));
        if let Some(lat) = lat {
            if let Some(lon) = lon {
                return Some(Location { lat, lon });
            }
        }

        return None;
    }
}

fn coord(coord: Option<i64>) -> Option<f64> {
    match coord {
        None | Some(-18000000000) => None,
        Some(x) => Some(x as f64 * 1e-8),
    }
}

pub async fn fetch(client: Client, bssid: String) -> Result<Vec<(Mac, Option<Location>)>> {
    let x = AppleWLoc {
        wifi_devices: vec![WifiDevice {
            bssid,
            location: None,
        }],
        ..Default::default()
    };

    let mut payload = Vec::new();

    // header
    payload.extend([0x00, 0x01, 0x00, 0x05]);
    payload.extend("en_US".as_bytes());
    payload.extend([0x00, 0x13]);
    payload.extend("com.apple.locationd".as_bytes());
    payload.extend([0x00, 0x0a]);
    payload.extend("17.5.21F79".as_bytes());
    payload.extend([0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);

    // request
    x.encode_length_delimited(&mut payload)?;
    x.encode(&mut payload)?;

    let resp = retry_fn(|| {
        let payload = payload.clone();
        let req = client
            .post("https://gs-loc.apple.com/clls/wloc")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "*/*")
            .header("accept-charset", "utf-8")
            .header("accept-language", "en-us")
            .header(
                "user-agent",
                "locationd/1753.17 CFNetwork/711.1.12 Darwin/14.0.0",
            )
            .body(payload);

        async { anyhow::Ok(req.send().await?.error_for_status()?) }
    })
    .retries(5)
    .await?;

    let x = AppleWLoc::decode(&resp.bytes().await?[10..])?;
    let mut output = Vec::new();
    for x in x.wifi_devices {
        let mac: Mac = match x.bssid.parse() {
            Ok(x) => x,
            Err(_) => bail!("Failed to parse mac: {}", x.bssid),
        };
        let loc = Location::from_raw(
            x.location
                .expect("response uses (-180, -180) to represent not found"),
        );
        output.push((mac, loc));
    }
    Ok(output)
}
