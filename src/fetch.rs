use std::{error::Error, time::Duration};

use anyhow::{bail, Result};
use h3o::{CellIndex, LatLng};
use mac6::Mac;
use mac_address::MacAddress;
use prost::Message;
use reqwest::Client;
use tryhard::retry_fn;

use crate::protobuf::{AppleWLoc, WifiDevice};

pub struct Location {
    pub lat: f64,
    pub lon: f64,
    pub acc: i64,
    pub alt: i64,
    pub alt_acc: i64,
}

impl Location {
    fn from_raw(value: crate::protobuf::Location) -> Option<Self> {
        let (lat, lon) = (value.latitude, value.longitude);
        let (lat, lon) = (coord(lat), coord(lon));
        if let Some(lat) = lat {
            if let Some(lon) = lon {
                return Some(Location {
                    lat,
                    lon,
                    acc: value.horizontal_accuracy.unwrap_or_default(),
                    alt: value.altitude.unwrap_or_default(),
                    alt_acc: value.vertical_accuracy.unwrap_or_default(),
                });
            }
        }

        None
    }

    pub fn cell(&self) -> (i64, i64) {
        let cell = CellIndex::from(
            LatLng::new(self.lat, self.lon)
                .unwrap()
                .to_cell(h3o::Resolution::Ten),
        );
        let cell3: u64 = cell.parent(h3o::Resolution::Six).unwrap().into();
        let cell: u64 = cell.into();
        (cell as i64, cell3 as i64)
    }
}

fn coord(coord: Option<i64>) -> Option<f64> {
    match coord {
        None | Some(-18000000000) => None,
        Some(x) => Some(x as f64 * 1e-8),
    }
}

pub async fn request(client: Client, x: AppleWLoc) -> Result<AppleWLoc> {
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
            .body(payload)
            .timeout(Duration::from_secs(10));

        async {
            let res = req.send().await.and_then(|x| x.error_for_status());
            if let Err(e) = &res {
                warn!("{:?}", e.source().unwrap_or(e))
            }
            res
        }
    })
    .retries(5)
    .await?;

    Ok(AppleWLoc::decode(&resp.bytes().await?[10..])?)
}

pub async fn fetch(client: Client, bssid: String) -> Result<Vec<(MacAddress, Option<Location>)>> {
    let x = request(
        client,
        AppleWLoc {
            wifi_devices: vec![WifiDevice {
                bssid,
                location: None,
            }],
            ..Default::default()
        },
    )
    .await?;

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
        output.push((MacAddress::new(mac.0), loc));
    }
    Ok(output)
}
