# Coverage models

`wtfps heatmap`, the HTTP API and the HTML map estimate where a cell is likely to serve phones. This page explains how, and what the two propagation models (`hata`, the default, and `simple`) do differently. The same model runs in two places that are kept in sync: in Rust (`src/coverage.rs`, for GeoJSON) and in the map's JavaScript (`src/heatmap.html`, so the controls work live).

It is an estimate, not a measurement: there are no antenna heights, powers, terrain or buildings in Apple's data, only each cell's estimated position, accuracy, channel (EARFCN) and an undocumented reach-like figure (field 12).

## The common core

Both models share everything except two things: whether frequency matters, and how far a cell reaches by default.

### Inputs

For the mapped cell (the *target*) and the cells it competes with:

- **position**: Apple's estimate of where the cell is used, not the tower. Sectors sit a little way out in the direction they face, on average 870 m from their site's centre.
- **accuracy** `acc` in metres (1414 m is the smallest Apple reports for cells).
- **EARFCN** (LTE channel), when Apple reported it.
- **field 12** (`unknown_12`), a reach-like distance in metres, used only by the experimental field 12 option.

By default the target only competes with cells **on its own EARFCN**: operators run several frequency layers over the same area, and a phone doesn't choose between layers by distance. With "all frequencies" (`--all-frequencies`, or unticking the map's checkbox) every cell of the operator competes.

### Signal from each cell

At a point at distance *d* from cell *j*, its received level in dB (up to a constant shared by all cells) is

```
P_j = −10 · n · log10( √(d² + (w · acc_j)² + 1 m²) )  −  L_f(j)  +  G_j
```

| Term | Meaning | Default |
|---|---|---|
| `n` | path-loss exponent: how fast signal falls with distance | 3.5 |
| `w · acc_j` | blurs the cell's position by its accuracy (`w`: accuracy weight); 0 takes positions as exact | `w` = 0 |
| `L_f(j)` | extra loss from cell *j*'s frequency relative to the target's | `hata`: Okumura–Hata's frequency term; `simple`: 0 |
| `G_j` | sector antenna gain towards the point | 0; with antenna directions, the 3GPP sector pattern: −12·(θ/65°)² dB, at most 20 dB down |

### Who serves a point

A phone uses the strongest cell, but real signal varies randomly from place to place with buildings and terrain (shadow fading, modelled with a standard deviation σ, default 8 dB). With that variation, the chance that the target is the strongest is a softmax over the levels:

```
likelihood = exp(P_target / T) / ( exp(P_edge / T) + Σ_j exp(P_j / T) ),     T = σ · √6 / π
```

`T` makes the logistic distribution behind the softmax match a Gaussian variation of σ dB. `P_edge` is a virtual "no service" competitor at the level the target has at its fade-out distance (*range*): beyond it, "no usable cell" wins more and more often, so coverage fades out gradually rather than ending at a line:

```
P_edge = −10 · n · log10(range)
```

Where nothing else competes, the likelihood is 50% at the range, about 15% at twice the range and about 6% at three times.

The map's two views:

- **Serving likelihood**: the formula above.
- **Relative signal**: `P_target − P_edge`, the target's level above its fade-out level in dB, ignoring other cells.

### Where it is evaluated, and the summary

- The map computes a 320 × 320 grid out to **3 × the range** around the cell (at least 1.5 km, at most 60 km), because the fade-out is gradual: a tighter area would cut the coverage off with a straight edge. It opens zoomed on 1.2 × the range.
- The GeoJSON evaluates H3 hexagons over the same area (resolution 10, about 0.015 km², coarser for large rural areas so a file has at most ~20,000) and keeps those with a likelihood of at least 1%.
- **Area** (`area_km2`) is where the likelihood is above 50%, i.e. where the cell is the most likely server; **reach** (`reach_m`) is the furthest such point. Both are measured on hexagons two resolutions finer than those written, so a small area isn't off by a whole hexagon.

## The two propagation models

| | `hata` (default) | `simple` |
|---|---|---|
| Frequency term `L_f` | Okumura–Hata / COST-231 frequency loss relative to the target | none: every frequency is equal |
| Default range | Hata's cell radius for the target's band and environment | 4 × the typical cell spacing |
| Needs | the target's EARFCN (otherwise falls back to `simple`'s range) | nothing beyond positions |
| Range shown as | `Okumura-Hata, 1855 MHz, urban` / "(Hata, urban)" | `4x cell spacing` / "(4× spacing)" |
| Range bounds | 0.5–60 km | 1.5–60 km |

Choose it with `--propagation hata|simple` (CLI), `?propagation=hata|simple` (HTTP API), or the "Propagation model" selector on the map. Setting a range yourself (`--range-km`, or the slider) overrides either model's default.

### `simple`: distance only

The model wtfps started with. Every cell transmits equally and only distance matters, so `L_f = 0`.

The range is **4 × the typical cell spacing**: the median distance from each cell (the nearest 300) to its nearest neighbour on the target's channel, ignoring neighbours under 30 m apart (sectors on the same mast), clamped to 1.5–60 km. The idea: where cells are packed tightly they are low-power and short-range, where they're sparse they reach further; the factor 4 makes the fade-out sit beyond the neighbours, so it's the competitors, not the fade-out, that shape the core.

### `hata`: Okumura–Hata and COST-231

[Okumura–Hata](https://en.wikipedia.org/wiki/Hata_model) is an empirical fit to measurements in Tokyo, the standard macro-cell path-loss model; COST-231 extended it from 1500 to 2000 MHz. With frequency *f* in MHz, mast height *h_b* and phone height *h_m* in metres and distance *d* in km, the urban path loss is

```
L = A(f) − 13.82·log10(h_b) − a(h_m) + (44.9 − 6.55·log10(h_b)) · log10(d)

A(f) = 69.55 + 26.16·log10(f)       for f ≤ 1500 MHz    (Okumura–Hata)
A(f) = 46.3  + 33.9 ·log10(f)       for f > 1500 MHz    (COST-231)

a(h_m) = (1.1·log10(f) − 0.7)·h_m − (1.56·log10(f) − 0.8)     (small or medium city)
```

with corrections outside towns:

```
suburban:  L − 2·(log10(f / 28))² − 5.4
rural:     L − 4.78·(log10 f)² + 18.33·log10(f) − 40.94
```

wtfps assumes **h_b = 30 m** and **h_m = 1.5 m**, and uses Hata in three ways:

**1. The distance slope matches the default exponent.** The distance term is 44.9 − 6.55·log10(h_b) dB per tenfold distance:

| Mast | Slope | Exponent `n` |
|---|---|---|
| 15 m | 37.2 dB/decade | 3.72 |
| 30 m | 35.2 dB/decade | **3.52** |
| 50 m | 33.8 dB/decade | 3.38 |

So the default `n` = 3.5 is Hata's urban slope for a 30 m mast. The suburban and rural corrections only subtract a constant, so the slope is the same outside towns. (An exponent of 2 is free space, which only applies over short line-of-sight paths.) Both models use `n` from the slider or `--path-loss`.

**2. Frequency term `L_f`.** Each cell's downlink frequency comes from its EARFCN (3GPP TS 36.101: the band's lowest downlink frequency plus 0.1 MHz per channel step), and it gets

```
L_f(j) = [A(f_j) − a(h_m, f_j) + C_env(f_j)] − [A(f_target) − a(h_m, f_target) + C_env(f_target)]
```

where `C_env` is the environment correction above. That is, the extra loss Hata gives cell *j* compared with the target at the same distance:

| Frequency | Extra loss compared with 800 MHz |
|---|---|
| 900 MHz (band 8) | +1.3 dB |
| 1800 MHz (band 3) | +11.1 dB |
| 2100 MHz (band 1) | +13.4 dB |
| 2600 MHz (band 7) | +16.5 dB |

When the target only competes with its own channel (the default), all cells share one frequency and `L_f` is zero for all of them: **`hata` and `simple` then differ only in the range**. With every frequency competing, `L_f` lets low-band cells carry further, as they do in reality. Cells whose EARFCN is unknown get `L_f = 0`.

**3. Default range.** The distance at which Hata's path loss reaches **140 dB**, a typical LTE maximum allowed path loss (link budget), at the target's frequency:

```
range = 10 ^ ( (140 − [A(f) − 13.82·log10(30) − a(1.5) + C_env(f)]) / 35.2 )  km
```

The environment is judged from the typical spacing of cells on the target's own channel (the same spacing as `simple` uses; on its own channel even with every frequency competing, because other bands' sectors on the same masts would make any place look dense):

| Typical spacing | Environment |
|---|---|
| under 800 m | urban |
| 800 m – 2 km | suburban |
| over 2 km | rural |

which gives:

| Band | Urban | Suburban | Rural |
|---|---|---|---|
| 800 MHz (band 20) | 2.7 km | 5.0 km | 16.6 km |
| 900 MHz (band 8) | 2.4 km | 4.7 km | 15.7 km |
| 1800 MHz (band 3) | 1.3 km | 2.8 km | 10.3 km |
| 2100 MHz (band 1) | 1.1 km | 2.5 km | 9.4 km |
| 2600 MHz (band 7) | 0.9 km | 2.1 km | 8.4 km |

clamped to 0.5–60 km. Without a known EARFCN for the target, `hata` falls back to `simple`'s range.

## Terrain

Optional, with either propagation model: `--terrain` (CLI), `?terrain=true` (HTTP API), and the "Terrain" checkbox on maps generated with it. It adds a term to each cell's level:

```
P_j = …  −  T_j(point)
```

`T_j` is the diffraction loss from the terrain between cell *j* and the point, from the [Copernicus GLO-30](https://spacedata.copernicus.eu/collections/copernicus-digital-elevation-model) elevation model (30 m grid, worldwide):

- **Geometry**: a 30 m mast at Apple's position of the cell, a phone 1.5 m above the ground at the point. The ground is lowered by the Earth's curvature, d² / (2 · 4/3 · 6371 km), the usual effective Earth radius for radio paths.
- **Dominant obstacle**: along each of 720 radial lines from the cell (every 0.5°), the terrain point rising highest as seen from the mast so far. A point behind it is shadowed by it.
- **Loss**: ITU-R P.526 single knife-edge diffraction. With *h* the obstacle's height above the straight line from mast to phone, *d₁* and *d₂* the distances from the mast to the obstacle and from the obstacle to the phone, and λ the wavelength at the cell's frequency:

  ```
  ν = h · √( 2 · (d₁ + d₂) / (λ · d₁ · d₂) )
  J(ν) = 6.9 + 20 · log10( √((ν − 0.1)² + 1) + ν − 0.1 )     for ν > −0.78, else 0
  ```

  So a path that clears every obstacle loses nothing, one grazing an edge loses about 6 dB, and one deep behind a ridge 20–30 dB or more.
- **Coverage**: computed for the mapped cell and its nearest competitors (30 on its own channel, 15 on others), on a 192 × 192 grid over a square covering the area either propagation model draws. Further cells, and anything outside that square, are treated as over flat ground.

Hata's formulas already include the *average* obstruction of their environment, so terrain adds the effect of *particular* hills on top; in flat areas it changes little, as it should.

| Cell | Without terrain | With terrain |
|---|---|---|
| Elena, Bulgaria, band 20, Balkan foothills | 13.39 km², reach 2.2 km, 618 hexagons ≥1% | 13.34 km², reach 3.4 km, 110 hexagons ≥1% |
| Kon Tum, band 3, flat town | 0.103 km², reach 225 m, 489 hexagons | 0.110 km², reach 240 m, 432 hexagons |

In the hills the core stays about the same size but changes shape (reaching further along open ground), and most of the faint fringe disappears behind ridges; on flat ground almost nothing changes.

**Limits**: one dominant obstacle per path (several ridges in a row are underestimated); the mast is placed at Apple's position, on average 870 m from the real site, and assumed 30 m high, so shadows near the cell are approximate; no buildings or vegetation; without measurements, the result is judged on plausibility (coverage following valleys and stopping at ridges), not checked against real signal.

## The two models compared

Three cells, same data, both models, with the target competing on its own channel (default) and with every frequency:

| Cell | Model | Range | Area, own channel | Area, all frequencies |
|---|---|---|---|---|
| Kon Tum, band 3, town (`452 4 38211 209991683`) | `hata` | 1.25 km (urban) | 0.103 km² | 0.020 km² |
| | `simple` | 2.42 km | 0.103 km² | 0.033 km² |
| Da Nang, band 3, coast (`452 1 36003 30883339`) | `hata` | 1.26 km (urban) | 0.187 km² | 0.045 km² |
| | `simple` | 1.86 km | 0.190 km² | 0.045 km² |
| Rural Bulgaria, band 20 (`284 1 26201 3945042`) | `hata` | 16.6 km (rural) | 13.39 km² | 3.47 km² |
| | `simple` | 8.22 km | 13.12 km² | 1.80 km² |

- **On its own channel the core is the same** in both models: competing cells, not the fade-out, decide where the target is the most likely server. The models differ in the faint fringe beyond the competitors and where nothing competes (out at sea in Da Nang, open country in Bulgaria).
- **With every frequency competing they differ a lot.** `simple` treats an 800 MHz and a 2600 MHz cell as equally strong; `hata` gives the 800 MHz one its real 9–17 dB advantage, so the rural 800 MHz cell's area nearly doubles (1.80 → 3.47 km²) and band-3 town cells shrink against low-band neighbours.
- **The ranges are in the same ballpark**, though they come from different reasoning: `simple`'s 2.4 km for Kon Tum sits between Hata's urban 1.3 km and suburban 2.8 km for band 3, and its 8.2 km for the rural cell is below Hata's rural 16.6 km for 800 MHz.

### Evidence from Apple's data

Apple's field 12 behaves like a reach estimate, and it follows Hata's frequency effect. Over 6,000 crawled cells, relative to how far apart the cells of each band are:

| Band | Median field 12 | Median spacing | Field 12 ÷ spacing |
|---|---|---|---|
| 800 MHz (band 20) | 21.6 km | 2.0 km | 10.2 |
| 900 MHz (band 8) | 14.6 km | 1.8 km | 8.6 |
| 1800 MHz (band 3) | 4.1 km | 0.8 km | 5.2 |
| 2100 MHz (band 1) | 5.6 km | 1.0 km | 5.3 |
| 2600 MHz (band 7) | 6.6 km | 1.0 km | 6.1 |

800 and 900 MHz cells reach 1.7–2 × further than 1800 and 2100 MHz cells; Hata predicts 1.6 × (rural) to 2.1 × (urban) for 800 against 1800 MHz. Band 7 is the outlier, plausibly because some channel numbers counted as band 7 (2937–3088) are 3G 900 MHz cells: Apple's data doesn't say which technology a cell uses.

## Which to use

- **`hata`** (default) when the cell's EARFCN is known, and always when comparing cells on different frequencies: it's the established model and Apple's own reach figures agree with its frequency effect.
- **`simple`** to reproduce earlier results, when the EARFCN is unknown anyway (both then behave the same), or as a check that doesn't depend on Hata's assumptions (30 m masts, a 140 dB link budget, the spacing thresholds for urban / suburban / rural).

## Limits

- **Hata's validity**: fitted for 150–1500 MHz (COST-231 to 2000 MHz), distances of 1–20 km and masts of 30–200 m. 2100 MHz and above are extrapolated; dense town cells a few hundred metres apart, often below rooftops, are outside its range, as they are outside the simple model's.
- **Fixed assumptions**: 30 m masts, phones at 1.5 m, a 140 dB link budget for every cell; real sites vary.
- **Positions are Apple's estimates** of where each cell is used, not tower locations.
- **No terrain or buildings**: shadow fading is random variation (σ), not geography; hills and rivers don't block anything.
- **Antenna directions** are optional and rough: estimated from where sectors sit relative to their site's centre.
- **LTE only**: frequencies, sites and sectors follow LTE numbering; see "Limits: LTE only" in the [README](../README.md#limits-lte-only).
- **Only known cells compete**: cells Apple doesn't know, or that haven't been crawled yet, are missing, so coverage looks larger where the crawl is thin.
