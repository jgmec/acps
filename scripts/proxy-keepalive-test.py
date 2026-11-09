#!/usr/bin/env python3
"""Check whether the server in WTFPS_APPLE_URL (e.g. a proxy) answers several lookups sent
over one kept-open connection, as `wtfps explore` does, and over new connections, as
`wtfps cell` does. Sends 10 read-only lookups of one cell.

    WTFPS_APPLE_URL=http://squid:3128/clls/wloc python3 scripts/proxy-keepalive-test.py
"""
import http.client
import os
import sys
import time
import urllib.parse

URL = os.environ.get("WTFPS_APPLE_URL", "https://gs-loc.apple.com/clls/wloc")
TIMEOUT = 10  # seconds, as wtfps


def varint(n):
    out = b""
    while True:
        b, n = n & 0x7F, n >> 7
        out += bytes([b | (0x80 if n else 0)])
        if not n:
            return out


def field(num, value):
    if isinstance(value, bytes):
        return varint(num << 3 | 2) + varint(len(value)) + value
    return varint(num << 3) + varint(value)


# the same request wtfps sends for `wtfps cell 284 5 106 451645`
cell = field(1, 284) + field(2, 5) + field(3, 451645) + field(4, 106)
message = field(25, cell)
header = (bytes([0, 1, 0, 5]) + b"en_US" + bytes([0, 0x13]) + b"com.apple.locationd"
          + bytes([0, 0x0A]) + b"17.5.21F79" + bytes([0, 0, 0, 1, 0, 0]))
BODY = header + len(message).to_bytes(2, "big") + message
HEADERS = {
    "content-type": "application/x-www-form-urlencoded",
    "accept": "*/*",
    "user-agent": "locationd/1753.17 CFNetwork/711.1.12 Darwin/14.0.0",
}

url = urllib.parse.urlsplit(URL)
path = url.path + (f"?{url.query}" if url.query else "")


def connect():
    cls = http.client.HTTPSConnection if url.scheme == "https" else http.client.HTTPConnection
    return cls(url.hostname, url.port, timeout=TIMEOUT)


def lookup(conn):
    start = time.monotonic()
    try:
        conn.request("POST", path, body=BODY, headers=HEADERS)
        response = conn.getresponse()
        data = response.read()
        result = f"HTTP {response.status}, {len(data)} bytes"
        if response.getheader("connection", "").lower() == "close":
            result += ", server closed the connection"
    except Exception as e:  # noqa: BLE001 - report any failure
        result = f"FAILED: {type(e).__name__} {e}"
    return f"{result} ({time.monotonic() - start:.1f} s)"


print(f"server: {URL}\n")
print("one kept-open connection (like wtfps explore):")
conn = connect()
for i in range(5):
    print(f"  request {i + 1}: {lookup(conn)}")
conn.close()

print("\na new connection for each request (like wtfps cell):")
for i in range(5):
    conn = connect()
    print(f"  request {i + 1}: {lookup(conn)}")
    conn.close()
sys.exit(0)
