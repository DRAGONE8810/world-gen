#!/usr/bin/env python3
"""Builds earth_relief.bin, the real-Earth relief used by the "Earth" planet.

Source: NOAA NCEI ETOPO 2022 15 arc-second Global Relief Model (ice surface),
doi:10.25921/fd45-gt74, public domain / free to redistribute. Fetched from
NOAA's ERDDAP server at a 3 arc-minute stride (every 12th sample), then
averaged 2 x 2 to 0.1 degree cells: 3600 x 1800, north-west corner first.

Encoding (decoded by crates/worldgen/src/realistic/earth.rs):
  b"WGER", u32 version=1, u32 width, u32 height, i32 land_step, i32 sea_step,
  then a zlib stream of varints: zigzag(value - prediction) per cell, row-major,
  where value = round(h / land_step) on land and -round(-h / sea_step) at sea,
  and prediction is the LOCO-I median edge detector over the decoded values.

Usage: python3 make_earth_relief.py OUT_DIR   (needs numpy, scipy, curl)
"""
import os, struct, subprocess, sys, zlib
import numpy as np
import scipy.io

BASE = "https://coastwatch.pfeg.noaa.gov/erddap/griddap/ETOPO_2022_v1_15s.nc"
LAND_STEP, SEA_STEP = 4, 16


def fetch(tmp):
    bands = []
    for b in range(16):
        hi = 90 - b * 11.25 - 0.0020833333
        lo = 90 - (b + 1) * 11.25 + 0.0020833333
        path = os.path.join(tmp, f"band_{b}.nc")
        if not os.path.exists(path):
            url = f"{BASE}?z%5B({lo}):12:({hi})%5D%5B(-179.99791666666667):12:(179.99791666666667)%5D"
            subprocess.run(["curl", "-s", "-m", "900", "-o", path, url], check=True)
        f = scipy.io.netcdf_file(path, "r", mmap=False)
        bands.append((f.variables["latitude"].data.copy(), f.variables["z"].data.astype(np.float64)))
    lat = np.concatenate([b[0] for b in bands])
    z = np.concatenate([b[1] for b in bands])
    return z[np.argsort(-lat)]  # north first


def encode(z):
    h, w = z.shape
    z = z[: h // 2 * 2, : w // 2 * 2].reshape(h // 2, 2, w // 2, 2).mean(axis=(1, 3))
    v = np.where(z >= 0, np.round(z / LAND_STEP), -np.round(-z / SEA_STEP)).astype(np.int64)
    a = np.zeros_like(v); b = np.zeros_like(v); c = np.zeros_like(v)
    a[:, 1:] = v[:, :-1]; b[1:, :] = v[:-1, :]; c[1:, 1:] = v[:-1, :-1]
    mx, mn = np.maximum(a, b), np.minimum(a, b)
    pred = np.where(c >= mx, mn, np.where(c <= mn, mx, a + b - c))
    r = (v - pred).ravel()
    zz = (r << 1) ^ (r >> 63)
    out = bytearray()
    for x in zz.tolist():
        while x >= 128:
            out.append((x & 127) | 128)
            x >>= 7
        out.append(x)
    hh, ww = v.shape
    return b"WGER" + struct.pack("<IIIii", 1, ww, hh, LAND_STEP, SEA_STEP) + zlib.compress(bytes(out), 9)


if __name__ == "__main__":
    out_dir = sys.argv[1] if len(sys.argv) > 1 else "."
    tmp = os.path.join(out_dir, "etopo_tmp")
    os.makedirs(tmp, exist_ok=True)
    data = encode(fetch(tmp))
    with open(os.path.join(out_dir, "earth_relief.bin"), "wb") as f:
        f.write(data)
    print(len(data), "bytes")
