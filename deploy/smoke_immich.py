#!/usr/bin/env python3
"""Live, temporary public-share test. Run on the host with a trusted .env file.

Never prints API credentials or the share key. Does not modify archive files.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("relative_path")
    parser.add_argument("--env-file", default=".env")
    args = parser.parse_args()
    config = {}
    for line in Path(args.env_file).read_text().splitlines():
        if line.strip() and not line.lstrip().startswith("#") and "=" in line:
            key, value = line.split("=", 1)
            config[key.strip()] = value.strip().strip("\"'")
    config.update(os.environ)
    root = Path(config["VIDEO_ROOT"]).resolve(strict=True)
    relative = Path(args.relative_path)
    assert not relative.is_absolute() and ".." not in relative.parts, "unsafe path"
    local = (root / relative).resolve(strict=True)
    assert local.is_relative_to(root), "path outside archive"
    original = config["IMMICH_CONTAINER_ROOT"].rstrip("/") + "/" + relative.as_posix()
    base = config["IMMICH_BASE_URL"].rstrip("/")
    public = config["IMMICH_PUBLIC_URL"].rstrip("/")
    show_metadata = config.get("IMMICH_SHOW_METADATA", "false").lower() == "true"

    def request(method, url, data=None, *, auth=False, prefix=None):
        headers = {"Accept": "*/*"}
        if auth:
            headers["x-api-key"] = config["IMMICH_API_KEY"]
        if data is not None:
            headers["Content-Type"] = "application/json"
            data = json.dumps(data).encode()
        if prefix:
            headers["Range"] = f"bytes=0-{prefix - 1}"
        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        with urllib.request.urlopen(req, timeout=40) as response:
            return response.status, response.read(prefix) if prefix else response.read()

    assets = []
    cursor = None
    while True:
        body = {"filter": {"originalPath": {"eq": original}},
                "orderBy": {"field": "fileCreatedAt", "direction": "desc"}}
        if cursor:
            body["cursor"] = cursor
        _, raw = request("POST", base + "/search/metadata", body, auth=True)
        result = json.loads(raw)["assets"]
        assets.extend(a for a in result["items"]
                      if a["originalPath"] == original
                      and a.get("libraryId") == config["IMMICH_LIBRARY_ID"]
                      and a["type"] == "VIDEO"
                      and not a["isOffline"] and not a["isTrashed"])
        cursor = result.get("nextCursor")
        if not cursor:
            break
    assert len(assets) == 1, "expected exactly one matching external video asset"
    asset_id = assets[0]["id"]
    print("Exact path, library and asset state: OK", flush=True)
    share_id = None
    try:
        expires = (datetime.datetime.now(datetime.timezone.utc)
                   + datetime.timedelta(minutes=5)).isoformat()
        _, raw = request("POST", base + "/shared-links", {
            "type": "INDIVIDUAL", "assetIds": [asset_id], "expiresAt": expires,
            "allowDownload": True, "allowUpload": False, "showMetadata": show_metadata,
            "description": "FPV deployment smoke " + str(uuid.uuid4()),
        }, auth=True)
        share = json.loads(raw)
        share_id = share["id"]
        key = urllib.parse.quote(share["key"], safe="")
        status, _ = request("GET", public + "/share/" + key)
        print(f"Public share page: HTTP {status}", flush=True)
        assert status == 200, "public share page unavailable"
        me_url = public + "/api/shared-links/me?key=" + key
        _, raw = request("GET", me_url)
        details = json.loads(raw)
        print("Public share checks: " + json.dumps({
            "allowDownload": details.get("allowDownload"),
            "allowUpload": details.get("allowUpload"),
            "showMetadata": details.get("showMetadata"),
            "asset_count": len(details.get("assets", [])),
            "selection_matches": {a["id"] for a in details.get("assets", [])} == {asset_id},
        }), flush=True)
        assert details["allowDownload"] is True
        assert details["allowUpload"] is False
        assert details["showMetadata"] is show_metadata
        assert {a["id"] for a in details["assets"]} == {asset_id}
        print("Public HTTPS share, exact selection and permissions: OK", flush=True)
        with local.open("rb") as source:
            expected = source.read(1024)
        status, actual = request("GET", public + "/api/assets/" + asset_id
                                 + "/original?key=" + key, prefix=len(expected))
        assert status == 206 and actual == expected, "original byte range mismatch"
        print("Public download of original: HTTP 206, bytes match source", flush=True)
    finally:
        if share_id:
            request("DELETE", base + "/shared-links/" + share_id, auth=True)
            print("Temporary share revoked", flush=True)
    try:
        request("GET", me_url)
    except urllib.error.HTTPError as error:
        assert error.code in (401, 403, 404), "unexpected revocation response"
        print("Revoked share denies access: OK", flush=True)
    else:
        raise AssertionError("revoked share still grants access")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # URLs in transport exceptions may contain the bearer key.
        status = f" HTTP {error.code}" if isinstance(error, urllib.error.HTTPError) else ""
        print(f"Smoke test failed: {type(error).__name__}{status}", file=sys.stderr)
        sys.exit(1)
