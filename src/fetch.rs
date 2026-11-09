use std::{error::Error, time::Duration};

use anyhow::{bail, Result};
use h3o::{CellIndex, LatLng, Resolution};
use mac6::Mac;
use mac_address::MacAddress;
use prost::Message;
use tryhard::retry_fn;

use crate::protobuf::{AppleWLoc, CellTower, NrCellTower, WifiDevice};

/// Apple's location service, and its server for China (`--china`).
const APPLE_URL: &str = "https://gs-loc.apple.com/clls/wloc";
const APPLE_CHINA_URL: &str = "https://gs-loc-cn.apple.com/clls/wloc";

#[derive(Clone)]
pub struct AppleWPSClient {
    client: reqwest::Client,
    url: String,
    /// a new connection for every request (going through a proxy)
    new_connections: bool,
}

impl AppleWPSClient {
    /// Apple's server, or with `china` its server for China; WTFPS_APPLE_URL and
    /// WTFPS_APPLE_CHINA_URL (environment or .env) replace them, e.g. for a proxy.
    ///
    /// Connections to Apple are kept open and reused. With any other URL (a proxy), each
    /// request gets a new connection instead: some proxies (e.g. Squid as a reverse proxy)
    /// stop answering requests sent over a reused connection, and the crawler then gets
    /// only timeouts.
    pub fn new(china: bool) -> Self {
        let (variable, default) =
            if china { ("WTFPS_APPLE_CHINA_URL", APPLE_CHINA_URL) } else { ("WTFPS_APPLE_URL", APPLE_URL) };
        let url = dotenvy::var(variable).ok().filter(|u| !u.is_empty()).unwrap_or_else(|| default.into());
        let new_connections = url != default;
        let mut builder = reqwest::Client::builder();
        if new_connections {
            info!("using {url}: a new connection for every request");
            builder = builder.pool_max_idle_per_host(0);
        }
        Self {
            client: builder.build().expect("HTTP client settings are valid"),
            url,
            new_connections,
        }
    }

    async fn query(&self, x: AppleWLoc) -> Result<AppleWLoc> {
        let body = self.query_raw(x).await?;
        Ok(AppleWLoc::decode(&body[10..])?)
    }

    /// The raw reply: a 10-byte header (ending in the u16 big-endian length of the rest)
    /// followed by the protobuf message.
    async fn query_raw(&self, x: AppleWLoc) -> Result<Vec<u8>> {
        let mut payload = Vec::new();

        // header
        payload.extend([0x00, 0x01, 0x00, 0x05]);
        payload.extend("en_US".as_bytes());
        payload.extend([0x00, 0x13]);
        payload.extend("com.apple.locationd".as_bytes());
        payload.extend([0x00, 0x0a]);
        payload.extend("17.5.21F79".as_bytes());
        payload.extend([0x00, 0x00, 0x00, 0x01, 0x00, 0x00]);

        // request: u16 big-endian length followed by the message
        payload.extend(u16::try_from(x.encoded_len())?.to_be_bytes());
        x.encode(&mut payload)?;

        let resp = retry_fn(|| {
            let payload = payload.clone();
            let mut req = self.client.post(&self.url);
            if self.new_connections {
                // tell the proxy too, so neither side waits on the old connection
                req = req.header("connection", "close");
            }
            let req = req
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

        let body = resp.bytes().await?;
        if body.len() < 10 {
            bail!("response too short: {} bytes", body.len());
        }
        Ok(body.to_vec())
    }

    pub async fn query_wifi(&self, mac: MacAddress) -> Result<Vec<(MacAddress, Option<Location>)>> {
        let x = self
            .query(AppleWLoc {
                wifi_devices: vec![WifiDevice {
                    bssid: mac.to_string(),
                    location: None,
                }],
                ..Default::default()
            })
            .await?;

        let mut output = Vec::new();
        for x in x.wifi_devices {
            let mac: Mac = match x.bssid.parse() {
                Ok(x) => x,
                Err(_) => bail!("Failed to parse mac: {}", x.bssid),
            };
            // not found is usually (-180, -180), but treat a missing location the same way
            let loc = x.location.and_then(Location::from_raw);
            output.push((MacAddress::new(mac.0), loc));
        }
        Ok(output)
    }

    pub async fn query_cell(&self, cell: CellTower) -> Result<Vec<(CellTower, Option<Location>)>> {
        debug!("querying cell: {:?}", cell);

        let x = self
            .query(AppleWLoc {
                cell_tower_request: vec![cell], ..Default::default() })
            .await?;

        let mut output = Vec::new();
        for x in x.cell_tower_response {
            output.push((x, x.location.and_then(Location::from_raw)));
        }
        Ok(output)
    }

    /// Raw reply for a cell request, for inspecting fields wtfps doesn't decode.
    pub async fn query_cell_raw(&self, cell: CellTower) -> Result<Vec<u8>> {
        self.query_raw(AppleWLoc { cell_tower_request: vec![cell], ..Default::default() })
            .await
    }

    /// A 5G NR cell and the nearby cells and tracking-area estimates Apple returns with it.
    pub async fn query_nr_cell(&self, cell: NrCellTower) -> Result<Vec<(NrCellTower, Option<Location>)>> {
        debug!("querying 5G cell: {:?}", cell);
        let reply = self.query(AppleWLoc { nr_cell_tower_request: vec![cell], ..Default::default() }).await?;
        Ok(reply.nr_cell_tower_response.into_iter().map(|c| (c, c.location.and_then(Location::from_raw))).collect())
    }

    /// Raw reply for a 5G NR cell request.
    pub async fn query_nr_cell_raw(&self, cell: NrCellTower) -> Result<Vec<u8>> {
        self.query_raw(AppleWLoc { nr_cell_tower_request: vec![cell], ..Default::default() }).await
    }
}

pub struct Location {
    pub lat: f64,
    pub lon: f64,
    pub acc: i64,
    /// altitude and vertical accuracy in metres, when the reply includes them (-1: unknown)
    pub alt: Option<i64>,
    pub alt_acc: Option<i64>,
    /// undocumented location fields 11 and 12, kept for analysis
    pub unknown_11: Option<i64>,
    pub unknown_12: Option<i64>,
}

impl Location {
    fn from_raw(value: crate::protobuf::Location) -> Option<Self> {
        // Apple uses (-180, -180) for "not found"
        let (lat, lon) = (coord(value.latitude)?, coord(value.longitude)?);
        Some(Location {
            lat,
            lon,
            acc: value.horizontal_accuracy.unwrap_or_default(),
            alt: value.altitude,
            alt_acc: value.vertical_accuracy,
            unknown_11: value.unknown_11,
            unknown_12: value.unknown_12,
        })
    }

    pub fn cell(&self, res: Resolution) -> CellIndex {
        LatLng::new(self.lat, self.lon).unwrap().to_cell(res)
    }
}

fn coord(coord: Option<i64>) -> Option<f64> {
    match coord {
        None | Some(-18000000000) => None,
        Some(x) => Some(x as f64 * 1e-8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf;

    #[test]
    fn coordinates_are_scaled_and_not_found_is_none() {
        assert_eq!(coord(Some(4_266_709_899)), Some(42.66709899));
        assert_eq!(coord(Some(-18_000_000_000)), None);
        assert_eq!(coord(None), None);
    }

    #[test]
    fn nr_requests_keep_the_whole_cell_id() {
        use prost::Message;
        let cell = protobuf::NrCellTower { mcc: 232, mnc: 1, cell_id: 17_248_763_906, tac_id: 17008, ..Default::default() };
        let request = AppleWLoc { nr_cell_tower_request: vec![cell], ..Default::default() };
        let bytes = request.encode_to_vec();
        // top-level field 29, length-delimited
        assert_eq!(&bytes[..2], &[0xea, 0x01]);
        let decoded = AppleWLoc::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.nr_cell_tower_request[0].cell_id, 17_248_763_906);
        assert!(decoded.cell_tower_request.is_empty());
    }

    #[test]
    fn location_from_reply() {
        let raw = protobuf::Location {
            latitude: Some(4_264_409_255),
            longitude: Some(2_330_012_512),
            horizontal_accuracy: Some(1414),
            unknown_12: Some(3200),
            ..Default::default()
        };
        let loc = Location::from_raw(raw).unwrap();
        assert_eq!((loc.lat, loc.lon, loc.acc, loc.unknown_12), (42.64409255, 23.30012512, 1414, Some(3200)));

        let not_found = protobuf::Location {
            latitude: Some(-18_000_000_000),
            longitude: Some(-18_000_000_000),
            horizontal_accuracy: Some(-1),
            ..Default::default()
        };
        assert!(Location::from_raw(not_found).is_none());
    }
}
