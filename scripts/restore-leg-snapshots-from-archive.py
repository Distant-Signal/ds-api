#!/usr/bin/env python3
# ruff: noqa: T201  # a CLI whose report is stdout
"""Restore tracked legs' train detail from the cold archive (DS-638).

A train leg's subscription loses `trains_id` when the aggregator prunes
`trains`; the leg then has no train uid and no timetable. ds-api's hourly
sweep (crates/api/src/data/leg_outcomes.rs) copies both onto the leg
(`journey_legs.final_train_uid`, `final_snapshot`, `final_snapshot_at`)
while the train exists, but legs pruned before it shipped have nothing. The
only place those trains still exist is the cold archive: zstd JSONL of
`trains` rows at `<prefix>/trains/service_date=YYYY-MM-DD/part-*.jsonl.zst`.

This script matches such legs to archived rows and writes SQL that restores
the same version 1 document the sweep writes (crates/api/src/data/
leg_snapshot.rs), with `source` `archive`. It NEVER writes to the database
or the archive: the SQL is for someone else to apply. See
docs/leg-snapshot-restore.md for the full runbook.

Matching (see `classify`): a leg is matched on its subscription's pin,

* service date: `journey_legs.service_date` (must equal the subscription's),
* origin: `pin_origin_crs`, else the leg's `origin_crs`,
* departure: `pin_scheduled_departure` (an instant),
* destination: `pin_destination_crs`, else the leg's `destination_crs`.

An archived row is a candidate when its own origin and `scheduled_departure`
equal the origin and departure, or (with --tiploc-map) when one of its
calling points is at that station and departs at that instant, which is how
a pin made at an intermediate station looks. A candidate must also reach
the destination: it is the row's own destination, or (with the map) a later
calling point. Exactly one such row: matched. None: unrecoverable. Several:
ambiguous, skipped. Rows found but none reaching the destination: a
mismatch, skipped. It never guesses.

Subcommands::

  dump-sql                 print the read-only SQL that lists the candidate legs
                           (and the TIPLOC to CRS map); run it with psql
  restore                  match legs to the archive; a dry run unless --emit-sql

Dry run, from a dump file (candidates.jsonl, tiploc.jsonl from `dump-sql`)::

  uv run scripts/restore-leg-snapshots-from-archive.py restore \
      --candidates candidates.jsonl --tiploc-map tiploc.jsonl

Or reading both from the database itself (read-only: SET default_transaction_
read_only, BEGIN READ ONLY) through kubectl exec::

  uv run scripts/restore-leg-snapshots-from-archive.py restore \
      --kubectl-context CTX --namespace NS --pg-pod POD --pg-db DB

The archive is read from S3 using the credentials the aggregator Deployment
names (same approach as the owner's archive helpers: read into memory, passed
to curl on stdin, never printed), or from a local mirror with --archive-dir.
Output is counts only; --verbose adds the opaque public ids of the legs in
each outcome. Nothing else about a leg is ever printed.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import dataclasses
import datetime as dt
import enum
import hashlib
import importlib
import io
import json
import os
import re
import subprocess
import sys
import xml.etree.ElementTree as ET
from collections import Counter
from pathlib import Path
from typing import IO, TYPE_CHECKING, Protocol
from urllib.parse import quote

if TYPE_CHECKING:
    from collections.abc import Iterable, Iterator, Sequence

SCRIPT_VERSION = 1
DOCUMENT_VERSION = 1
DEFAULT_BATCH = 200
S3_NS = {"s3": "http://s3.amazonaws.com/doc/2006-03-01/"}
KEY_RE = re.compile(r"service_date=(\d{4}-\d{2}-\d{2})/[^/]+\.jsonl(\.zst)?$")

# The read-only candidate dump: every train leg whose subscription lost its
# train, one JSON object per line. Includes legs that already have a snapshot
# (`hasSnapshot`) so the report can count them as skipped. No user id, name
# or free text is selected.
CANDIDATES_SQL = """\
SELECT json_build_object(
    'legId', jl.id,
    'publicId', jl.public_id,
    'subscriptionId', ts.id,
    'serviceDate', jl.service_date,
    'subServiceDate', ts.service_date,
    'legOrigin', jl.origin_crs,
    'legDestination', jl.destination_crs,
    'pinOrigin', ts.pin_origin_crs,
    'pinDestination', ts.pin_destination_crs,
    'pinDeparture', ts.pin_scheduled_departure,
    'recorded', jl.final_recorded_at IS NOT NULL,
    'delayMinutes', jl.final_arrival_delay_minutes,
    'cancelled', jl.final_train_cancelled,
    'hasSnapshot', jl.final_snapshot IS NOT NULL
)
FROM journey_legs jl
JOIN train_subscriptions ts ON ts.id = jl.train_subscription_id
WHERE jl.kind = 'train'
  AND ts.trains_id IS NULL
  AND ts.resolution_status IN ('resolved', 'schedule_matched')
ORDER BY jl.id;
"""

TIPLOC_SQL = """\
SELECT json_build_object('tiploc', tiploc, 'crs', crs)
FROM (
    SELECT tiploc, crs FROM tiploc_crs
    UNION
    SELECT tiploc, crs FROM corpus_tiploc_crs
) m
ORDER BY tiploc;
"""


class Outcome(enum.Enum):
    """What happened to one candidate leg."""

    MATCHED = "matched"
    AMBIGUOUS = "ambiguous"
    MISMATCHED = "mismatched"
    OLDER = "unrecoverable, older than the earliest archived service date"
    NEWER = "unrecoverable, newer than the latest archived service date"
    NO_ROW = "unrecoverable, within the archive range but no row"
    NO_KEY = "unrecoverable, no recorded origin or departure to match on"
    UNUSABLE = "unusable archived row (no calling points, or text jsonb rejects)"
    SNAPSHOTTED = "already snapshotted"


@dataclasses.dataclass(frozen=True)
class Candidate:
    """One train leg without its `trains` row (a line of the candidate dump)."""

    leg_id: int
    public_id: str | None
    subscription_id: int
    service_date: dt.date
    sub_service_date: dt.date | None
    origin: str | None
    destination: str | None
    departure: dt.datetime | None
    recorded: bool
    delay_minutes: int | None
    cancelled: bool | None
    has_snapshot: bool

    @property
    def label(self) -> str:
        """The opaque id --verbose may print."""
        return self.public_id or f"id:{self.leg_id}"


@dataclasses.dataclass(frozen=True)
class ArchiveRow:
    """One archived `trains` row, parsed."""

    train_uid: str
    service_date: dt.date
    origin: str | None
    destination: str | None
    departure: dt.datetime | None
    calling_points: tuple[dict[str, object], ...]
    raw: dict[str, object]


@dataclasses.dataclass(frozen=True)
class Hit:
    """An archived row that departs the candidate's origin at its time."""

    row: ArchiveRow
    index: int  # 0 for the row's own origin, else the calling point's index


class EndCheck(enum.Enum):
    """How a row reaches a leg's destination."""

    ROW_END = "the row's own destination"
    CALLING = "a later calling point"
    UNCHECKED = "the leg records no destination"
    FAIL = "not reached"


Key = tuple[dt.date, str, dt.datetime]


def crs_of(value: object) -> str | None:
    """Return a trimmed upper-case CRS, or None."""
    if not isinstance(value, str):
        return None
    text = value.strip().upper()
    return text or None


def parse_instant(value: object) -> dt.datetime | None:
    """Parse an RFC 3339 or PostgreSQL JSON timestamp to an aware UTC datetime."""
    if not isinstance(value, str):
        return None
    text = value.strip().replace(" ", "T", 1)
    if text.endswith(("Z", "z")):
        text = text[:-1] + "+00:00"
    if re.search(r"[+-]\d{2}$", text):
        text += ":00"
    try:
        parsed = dt.datetime.fromisoformat(text)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return None
    return parsed.astimezone(dt.UTC)


def parse_date(value: object) -> dt.date | None:
    """Parse a `YYYY-MM-DD` date."""
    if not isinstance(value, str):
        return None
    try:
        return dt.date.fromisoformat(value[:10])
    except ValueError:
        return None


def parse_candidate(obj: dict[str, object]) -> Candidate | None:
    """Parse a candidate dump line; None when it lacks what even a count needs."""
    leg_id = obj.get("legId")
    subscription_id = obj.get("subscriptionId")
    service_date = parse_date(obj.get("serviceDate"))
    if (
        not isinstance(leg_id, int)
        or not isinstance(subscription_id, int)
        or service_date is None
    ):
        return None
    delay = obj.get("delayMinutes")
    cancelled = obj.get("cancelled")
    public_id = obj.get("publicId")
    return Candidate(
        leg_id=leg_id,
        public_id=public_id if isinstance(public_id, str) else None,
        subscription_id=subscription_id,
        service_date=service_date,
        sub_service_date=parse_date(obj.get("subServiceDate")),
        origin=crs_of(obj.get("pinOrigin")) or crs_of(obj.get("legOrigin")),
        destination=crs_of(obj.get("pinDestination"))
        or crs_of(obj.get("legDestination")),
        departure=parse_instant(obj.get("pinDeparture")),
        recorded=obj.get("recorded") is True,
        delay_minutes=delay
        if isinstance(delay, int) and not isinstance(delay, bool)
        else None,
        cancelled=cancelled if isinstance(cancelled, bool) else None,
        has_snapshot=obj.get("hasSnapshot") is True,
    )


def parse_row(obj: dict[str, object]) -> ArchiveRow | None:
    """Parse an archived row; None when it has no uid or service date."""
    uid = obj.get("train_uid")
    service_date = parse_date(obj.get("service_date"))
    if not isinstance(uid, str) or not uid or service_date is None:
        return None
    points = obj.get("calling_points")
    if isinstance(points, str):  # a JSONB column exported as text
        with contextlib.suppress(ValueError):
            points = json.loads(points)
    calling = (
        tuple(p for p in points if isinstance(p, dict))
        if isinstance(points, list)
        else ()
    )
    return ArchiveRow(
        train_uid=uid,
        service_date=service_date,
        origin=crs_of(obj.get("origin_crs")),
        destination=crs_of(obj.get("destination_crs")),
        departure=parse_instant(obj.get("scheduled_departure")),
        calling_points=calling,
        raw=obj,
    )


def london_to_utc(
    day: dt.date, time_text: object, day_offset: object
) -> dt.datetime | None:
    """Convert a calling point's booked London time to a UTC instant."""
    from zoneinfo import ZoneInfo  # noqa: PLC0415  # only the calling-point tier needs tzdata

    if not isinstance(time_text, str):
        return None
    for fmt in ("%H:%M:%S", "%H:%M"):
        try:
            clock = dt.datetime.strptime(time_text, fmt).time()  # noqa: DTZ007  # naive, localised below
        except ValueError:
            continue
        offset = (
            day_offset
            if isinstance(day_offset, int) and not isinstance(day_offset, bool)
            else 0
        )
        local = dt.datetime.combine(
            day + dt.timedelta(days=offset), clock, tzinfo=ZoneInfo("Europe/London")
        )
        return local.astimezone(dt.UTC)
    return None


def departure_events(
    row: ArchiveRow, tiplocs: dict[str, str]
) -> Iterator[tuple[str, dt.datetime, int]]:
    """Every (crs, instant, index) at which the row departs a station.

    Index 0 is the row's own origin and scheduled departure; the rest are the
    calling points that name a CRS in `tiplocs` and have a booked or public
    departure.
    """
    if row.origin is not None and row.departure is not None:
        yield row.origin, row.departure, 0
    for index, point in enumerate(row.calling_points, start=1):
        tiploc = point.get("tiploc")
        crs = tiplocs.get(tiploc.upper()) if isinstance(tiploc, str) else None
        if crs is None:
            continue
        seen: set[dt.datetime] = set()
        for field in ("bookedDeparture", "publicDeparture"):
            instant = london_to_utc(
                row.service_date, point.get(field), point.get("dayOffset")
            )
            if instant is not None and instant not in seen:
                seen.add(instant)
                yield crs, instant, index


def candidate_key(cand: Candidate) -> Key | None:
    """Return the (service date, origin, departure) a leg is matched on."""
    if cand.origin is None or cand.departure is None:
        return None
    return cand.service_date, cand.origin, cand.departure


def end_check(
    row: ArchiveRow, hit_index: int, destination: str | None, tiplocs: dict[str, str]
) -> EndCheck:
    """Whether the row reaches `destination` after the hit's station."""
    if destination is None:
        return EndCheck.UNCHECKED
    if row.destination == destination:
        return EndCheck.ROW_END
    for point in row.calling_points[max(hit_index, 0) :]:
        tiploc = point.get("tiploc")
        if isinstance(tiploc, str) and tiplocs.get(tiploc.upper()) == destination:
            return EndCheck.CALLING
    return EndCheck.FAIL


def usable(row: ArchiveRow, cand: Candidate) -> bool:
    """Whether the row can make a document the sweep would also write.

    The sweep needs calling points unless the leg's outcome is recorded; and
    jsonb refuses U+0000, which would fail a whole batch.
    """
    if "\\u0000" in json.dumps(row.raw) or "\x00" in row.train_uid:
        return False
    return bool(row.calling_points) or cand.recorded


def classify(  # noqa: PLR0911  # one return per outcome reads clearest
    cand: Candidate,
    hits: Sequence[Hit],
    tiplocs: dict[str, str],
    archive_range: tuple[dt.date, dt.date] | None,
) -> tuple[Outcome, Hit | None, EndCheck | None]:
    """Decide one candidate's outcome from the rows departing its origin then."""
    if cand.has_snapshot:
        return Outcome.SNAPSHOTTED, None, None
    if cand.sub_service_date is not None and cand.sub_service_date != cand.service_date:
        return Outcome.MISMATCHED, None, None
    if candidate_key(cand) is None:
        return Outcome.NO_KEY, None, None
    if not hits:
        if archive_range is not None and cand.service_date < archive_range[0]:
            return Outcome.OLDER, None, None
        if archive_range is not None and cand.service_date > archive_range[1]:
            return Outcome.NEWER, None, None
        return Outcome.NO_ROW, None, None
    reaching = [
        (hit, end_check(hit.row, hit.index, cand.destination, tiplocs)) for hit in hits
    ]
    reaching = [(hit, check) for hit, check in reaching if check is not EndCheck.FAIL]
    if not reaching:
        return Outcome.MISMATCHED, None, None
    if len(reaching) > 1:
        return Outcome.AMBIGUOUS, None, None
    hit, check = reaching[0]
    if not usable(hit.row, cand):
        return Outcome.UNUSABLE, None, check
    return Outcome.MATCHED, hit, check


def rfc3339(value: dt.datetime) -> str:
    """UTC with `Z` and chrono's automatic fraction (none, 3 or 6 digits)."""
    value = value.astimezone(dt.UTC)
    text = value.strftime("%Y-%m-%dT%H:%M:%S")
    micro = value.microsecond
    if micro:
        text += f".{micro:06d}" if micro % 1000 else f".{micro // 1000:03d}"
    return text + "Z"


def build_document(
    row: ArchiveRow, cand: Candidate, captured_at: dt.datetime
) -> dict[str, object]:
    """Build the version 1 `LegSnapshot` for `row`, source `archive`.

    Unknown keys are absent, never null. With the leg's outcome recorded, the
    outcome keys come from the leg.
    """
    raw = row.raw
    doc: dict[str, object] = {
        "v": DOCUMENT_VERSION,
        "source": "archive",
        "capturedAt": rfc3339(captured_at),
        "trainUid": row.train_uid,
        "serviceDate": row.service_date.isoformat(),
    }

    def text(key: str, name: str) -> None:
        value = raw.get(name)
        if isinstance(value, str) and value:
            doc[key] = value

    text("trainId", "train_id")
    if row.origin is not None:
        doc["originCrs"] = row.origin
    if row.destination is not None:
        doc["destinationCrs"] = row.destination
    if row.departure is not None:
        doc["scheduledDeparture"] = rfc3339(row.departure)
    text("matchedLineId", "matched_line_id")
    text("platform", "platform")
    text("plannedPlatform", "planned_platform")
    skipped = raw.get("skipped_stations")
    if (
        isinstance(skipped, list)
        and skipped
        and all(isinstance(s, str) for s in skipped)
    ):
        doc["skippedStations"] = skipped
    doc["callingPoints"] = list(row.calling_points)
    if cand.recorded:
        if cand.delay_minutes is not None:
            doc["finalArrivalDelayMinutes"] = cand.delay_minutes
        if cand.cancelled is not None:
            doc["trainCancelled"] = cand.cancelled
    return doc


def dollar_quote(text: str) -> str:
    """`text` as a dollar-quoted literal whose tag does not occur in it.

    Dollar quotes need no escaping and, unlike '...', do not depend on
    standard_conforming_strings. NUL cannot appear in a PostgreSQL text.
    """
    if "\x00" in text:
        message = "NUL in a SQL text"
        raise ValueError(message)
    n = 0
    while True:
        tag = f"$ds{n}$"
        if tag not in text and not text.endswith(f"$ds{n}"):
            return f"{tag}{text}{tag}"
        n += 1


def statement(cand: Candidate, doc: dict[str, object]) -> str:
    """Render the idempotent UPDATE for one leg.

    Applies only while the leg still has no snapshot, still has the
    subscription it was read with, that subscription still has no train, and
    the leg's outcome is still (not) recorded as when the document was made.
    `final_snapshot_at` is now(), and strictly after `final_recorded_at` when
    set: ds-api's sweep re-snapshots a leg whose snapshot predates its outcome.
    """
    document = json.dumps(doc, separators=(",", ":"), sort_keys=True)
    recorded = "IS NOT NULL" if cand.recorded else "IS NULL"
    return (
        "UPDATE journey_legs AS jl SET "  # noqa: S608  # ints and dollar-quoted literals only
        f"final_train_uid = {dollar_quote(str(doc['trainUid']))}, "
        f"final_snapshot = {dollar_quote(document)}::jsonb, "
        "final_snapshot_at = GREATEST(now(), "
        "COALESCE(jl.final_recorded_at, '-infinity'::timestamptz) "
        "+ interval '1 second') "
        "FROM train_subscriptions AS ts "
        f"WHERE jl.id = {cand.leg_id} AND jl.kind = 'train' "
        "AND jl.final_snapshot IS NULL "
        f"AND jl.train_subscription_id = {cand.subscription_id} "
        f"AND ts.id = {cand.subscription_id} "
        f"AND ts.trains_id IS NULL AND jl.final_recorded_at {recorded};"
    )


def batches(
    items: Sequence[tuple[Candidate, dict[str, object]]], size: int
) -> list[str]:
    """One transaction per `size` legs."""
    out = []
    for start in range(0, len(items), size):
        chunk = items[start : start + size]
        lines = [
            "BEGIN;",
            "SET LOCAL lock_timeout = '5s';",
            "SET LOCAL statement_timeout = '120s';",
        ]
        lines.extend(statement(cand, doc) for cand, doc in chunk)
        lines.append("COMMIT;")
        out.append("\n".join(lines) + "\n")
    return out


def id_array(ids: Iterable[int]) -> str:
    """Render a SQL bigint array literal."""
    return "ARRAY[" + ",".join(str(i) for i in sorted(ids)) + "]::bigint[]"


def verify_sql(ids: Sequence[int]) -> str:
    """Render the trailing read-only check that restored legs read back right."""
    return (
        "-- Read-only. restored must equal expected; the other two must be 0.\n"
        "SELECT count(*) AS expected,\n"
        "  count(*) FILTER (WHERE jl.final_snapshot->>'source' = 'archive'\n"
        "    AND (jl.final_snapshot->>'v')::int = 1\n"
        "    AND jl.final_train_uid = jl.final_snapshot->>'trainUid') AS restored,\n"
        "  count(*) FILTER (WHERE jl.final_snapshot IS NULL)\n"
        "    AS still_unsnapshotted,\n"
        "  count(*) FILTER (WHERE jl.final_recorded_at IS NOT NULL\n"
        "    AND jl.final_snapshot_at <= jl.final_recorded_at)\n"
        "    AS stale_against_outcome\n"
        f"FROM journey_legs jl WHERE jl.id = ANY({id_array(ids)});\n"
    )


def rollback_sql(ids: Sequence[int], run_at: dt.datetime) -> str:
    """Undo: clear the three columns of the legs this run wrote."""
    return (
        f"-- Undo run {rfc3339(run_at)}: only archive restores.\n"
        "BEGIN;\n"
        "UPDATE journey_legs SET final_train_uid = NULL, final_snapshot = NULL,\n"
        "  final_snapshot_at = NULL\n"
        f"WHERE id = ANY({id_array(ids)})\n"
        "  AND final_snapshot->>'source' = 'archive'\n"
        f"  AND final_snapshot_at >= {dollar_quote(rfc3339(run_at))}::timestamptz;\n"
        "COMMIT;\n"
    )


def emit(  # noqa: PLR0913, PLR0917  # the run's facts, each used once
    out_dir: Path,
    items: Sequence[tuple[Candidate, dict[str, object]]],
    counts: Counter[Outcome],
    run_at: dt.datetime,
    size: int,
    archive_range: tuple[dt.date, dt.date] | None,
) -> tuple[int, int]:
    """Write the batch files, verify.sql, rollback.sql and manifest.json.

    The manifest holds counts and checksums only.

    Returns the number of files written and their total size in bytes.
    """
    out_dir.mkdir(parents=True, exist_ok=True)
    files = []
    for number, text in enumerate(batches(items, size), start=1):
        name = f"batch-{number:04d}.sql"
        data = text.encode()
        write_private(out_dir / name, data)
        files.append(
            {
                "name": name,
                "legs": text.count("UPDATE journey_legs"),
                "bytes": len(data),
                "sha256": hashlib.sha256(data).hexdigest(),
            },
        )
    ids = [cand.leg_id for cand, _ in items]
    extras = {"verify.sql": verify_sql(ids), "rollback.sql": rollback_sql(ids, run_at)}
    for name, text in extras.items():
        data = text.encode()
        write_private(out_dir / name, data)
        files.append(
            {
                "name": name,
                "legs": len(ids),
                "bytes": len(data),
                "sha256": hashlib.sha256(data).hexdigest(),
            },
        )
    manifest: dict[str, object] = {
        "script": "restore-leg-snapshots-from-archive.py",
        "scriptVersion": SCRIPT_VERSION,
        "documentVersion": DOCUMENT_VERSION,
        "generatedAt": rfc3339(run_at),
        "batchSize": size,
        "archiveRange": [d.isoformat() for d in archive_range]
        if archive_range
        else None,
        "counts": {outcome.name.lower(): counts[outcome] for outcome in Outcome},
        "legsToApply": len(ids),
        "totalBytes": sum(int(str(f["bytes"])) for f in files),
        "files": files,
    }
    write_private(
        out_dir / "manifest.json", (json.dumps(manifest, indent=2) + "\n").encode()
    )
    return len(files) + 1, int(str(manifest["totalBytes"]))


def write_private(path: Path, data: bytes) -> None:
    """Write `path` readable by the owner only (the SQL names legs)."""
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "wb") as handle:
        handle.write(data)


# --- archive access ------------------------------------------------------


class Archive(Protocol):
    """Where the archived `trains` rows are read from."""

    def listing(self) -> dict[dt.date, list[str]]:
        """Every service date and the object keys under it."""
        ...

    def rows(self, key: str) -> Iterator[dict[str, object]]:
        """Yield the JSON objects of one object, one at a time."""
        ...


def decode_lines(raw: bytes, name: str) -> Iterator[bytes]:
    """Yield the lines of a possibly zstd-compressed JSONL object."""
    if not name.endswith(".zst"):
        yield from io.BytesIO(raw)
        return
    try:
        zstd = importlib.import_module("compression.zstd")  # Python 3.14+
    except ImportError:
        zstd = None
    if zstd is not None:
        with zstd.ZstdFile(io.BytesIO(raw)) as handle:
            yield from handle
        return
    done = subprocess.run(["zstd", "-dc"], input=raw, capture_output=True, check=False)  # noqa: S607
    if done.returncode != 0:
        message = f"zstd decompression failed for {name}"
        raise SystemExit(message)
    yield from io.BytesIO(done.stdout)


def json_objects(
    lines: Iterable[bytes], unreadable: list[int]
) -> Iterator[dict[str, object]]:
    """JSON objects from lines; counts, never shows, a line that is not one."""
    for line in lines:
        if not line.strip():
            continue
        try:
            obj = json.loads(line)
        except ValueError:
            unreadable[0] += 1
            continue
        if isinstance(obj, dict):
            yield obj
        else:
            unreadable[0] += 1


class LocalArchive:
    """A directory with the archive's layout (service_date=.../part-*.jsonl[.zst])."""

    def __init__(self, root: Path, unreadable: list[int]) -> None:
        """Index `root`."""
        self.root = root
        self.unreadable = unreadable

    def listing(self) -> dict[dt.date, list[str]]:
        """Every service date and its files."""
        out: dict[dt.date, list[str]] = {}
        for path in sorted(self.root.rglob("*.jsonl*")):
            key = path.relative_to(self.root).as_posix()
            match = KEY_RE.search(key)
            if match:
                out.setdefault(dt.date.fromisoformat(match.group(1)), []).append(key)
        return out

    def rows(self, key: str) -> Iterator[dict[str, object]]:
        """One file's rows."""
        raw = (self.root / key).read_bytes()
        return json_objects(decode_lines(raw, key), self.unreadable)


@dataclasses.dataclass(frozen=True)
class Conn:
    """The archive's S3 endpoint and credentials, held in memory only."""

    endpoint: str
    bucket: str
    prefix: str
    region: str
    access_key: str = dataclasses.field(repr=False)
    secret_key: str = dataclasses.field(repr=False)


def run_text(
    cmd: Sequence[str], *, stdin: str | None = None, env: dict[str, str] | None = None
) -> str:
    """Run a command; on failure say which one and a clipped stderr, never its stdin."""
    done = subprocess.run(  # noqa: S603  # fixed argv
        list(cmd),
        capture_output=True,
        text=True,
        input=stdin,
        env=env,
        timeout=600,
        check=False,
    )
    if done.returncode != 0:
        clipped = done.stderr.strip()[:300]
        message = f"command failed ({cmd[0]} rc={done.returncode}): {clipped}"
        raise SystemExit(message)
    return done.stdout


def kube_env() -> dict[str, str]:
    """Return the environment with the tailnet SOCKS proxy defaulted."""
    env = dict(os.environ)
    env.setdefault("HTTPS_PROXY", "socks5://127.0.0.1:1055")
    return env


def kubectl(context: str, namespace: str, *args: str, stdin: str | None = None) -> str:
    """Kubectl in a namespace."""
    return run_text(
        ["kubectl", "--context", context, "-n", namespace, *args],
        stdin=stdin,
        env=kube_env(),
    )


def load_conn(context: str, namespace: str, deployment: str) -> Conn:
    """Read the archive settings from the aggregator Deployment and its Secret."""
    dep = json.loads(
        kubectl(context, namespace, "get", "deploy", deployment, "-o", "json")
    )
    env = {
        e["name"]: e
        for e in dep["spec"]["template"]["spec"]["containers"][0].get("env", [])
    }

    def literal(name: str) -> str:
        entry = env.get(name)
        if entry is None or "value" not in entry:
            message = f"{name} is not a literal env var on the {deployment} Deployment"
            raise SystemExit(message)
        return str(entry["value"])

    def secret_value(name: str) -> str:
        ref = env.get(name, {}).get("valueFrom", {}).get("secretKeyRef")
        if not ref:
            message = f"{name} has no secretKeyRef on the {deployment} Deployment"
            raise SystemExit(message)
        raw = kubectl(
            context,
            namespace,
            "get",
            "secret",
            str(ref["name"]),
            "-o",
            f"jsonpath={{.data.{ref['key']}}}",
        )
        return base64.b64decode(raw).decode().strip()

    return Conn(
        endpoint=literal("ARCHIVE_S3_ENDPOINT").rstrip("/"),
        bucket=literal("ARCHIVE_S3_BUCKET"),
        prefix=literal("ARCHIVE_S3_PREFIX").strip("/"),
        region=env.get("ARCHIVE_S3_REGION", {}).get("value", "us-east-1"),
        access_key=secret_value("ARCHIVE_S3_ACCESS_KEY_ID"),
        secret_key=secret_value("ARCHIVE_S3_SECRET_ACCESS_KEY"),
    )


def curl_get(conn: Conn, proxy: str, url: str) -> bytes:
    """Do a signed GET; the credentials go to curl on stdin, never argv."""
    config = f'user = "{conn.access_key}:{conn.secret_key}"\n'
    done = subprocess.run(  # noqa: S603  # fixed argv; credentials via stdin
        [  # noqa: S607
            "curl",
            "-sS",
            "--fail-with-body",
            "--max-time",
            "120",
            "--proxy",
            proxy,
            "--aws-sigv4",
            f"aws:amz:{conn.region}:s3",
            "-K",
            "-",
            url,
        ],
        input=config.encode(),
        capture_output=True,
        timeout=180,
        check=False,
    )
    if done.returncode != 0:
        clipped = done.stderr.decode(errors="replace")[:200]
        message = f"archive request failed (curl rc={done.returncode}): {clipped}"
        raise SystemExit(message)
    return done.stdout


def parse_s3_list(xml_text: str) -> tuple[list[str], str | None]:
    """Object keys and the continuation token from a ListObjectsV2 page."""
    root = ET.fromstring(xml_text)  # noqa: S314  # the owner's own bucket
    keys = [c.findtext("s3:Key", "", S3_NS) for c in root.findall("s3:Contents", S3_NS)]
    return keys, root.findtext("s3:NextContinuationToken", None, S3_NS)


class S3Archive:
    """The owner's S3 archive, read through curl over the tailnet proxy."""

    def __init__(self, conn: Conn, proxy: str, unreadable: list[int]) -> None:
        """Hold the connection."""
        self.conn = conn
        self.proxy = proxy
        self.unreadable = unreadable

    def listing(self) -> dict[dt.date, list[str]]:
        """List every object under `<prefix>/trains/` (all service dates)."""
        base = f"{self.conn.prefix}/trains/"
        out: dict[dt.date, list[str]] = {}
        token: str | None = None
        while True:
            query = f"list-type=2&prefix={quote(base, safe='/')}"
            if token:
                query += f"&continuation-token={quote(token, safe='')}"
            page = curl_get(
                self.conn,
                self.proxy,
                f"{self.conn.endpoint}/{self.conn.bucket}?{query}",
            )
            keys, token = parse_s3_list(page.decode())
            for key in keys:
                relative = key.removeprefix(f"{self.conn.prefix}/")
                match = KEY_RE.search(relative)
                if match:
                    out.setdefault(dt.date.fromisoformat(match.group(1)), []).append(
                        relative
                    )
            if not token:
                return out

    def rows(self, key: str) -> Iterator[dict[str, object]]:
        """Download one object and yield its rows."""
        url = f"{self.conn.endpoint}/{self.conn.bucket}/{self.conn.prefix}/{key}"
        raw = curl_get(self.conn, self.proxy, url)
        return json_objects(decode_lines(raw, key), self.unreadable)


# --- the run -------------------------------------------------------------


@dataclasses.dataclass
class Scan:
    """What reading the archive produced."""

    hits: dict[Key, dict[tuple[str, dt.date], Hit]]
    objects: int
    dates_read: int
    rows_scanned: int


def scan_archive(
    archive: Archive,
    listing: dict[dt.date, list[str]],
    wanted: set[Key],
    tiplocs: dict[str, str],
) -> Scan:
    """Read every object of every service date any candidate is on.

    Only rows that depart a wanted (date, origin, instant) are kept, so memory
    is bounded by the candidates, not the archive; and a row that is not kept
    cannot make any candidate ambiguous. Duplicate (train_uid, service_date)
    rows (an object written twice) count once.
    """
    dates = {key[0] for key in wanted}
    hits: dict[Key, dict[tuple[str, dt.date], Hit]] = {}
    scan = Scan(
        hits=hits,
        objects=sum(len(v) for v in listing.values()),
        dates_read=0,
        rows_scanned=0,
    )
    for day in sorted(dates & set(listing)):
        scan.dates_read += 1
        for key in listing[day]:
            for obj in archive.rows(key):
                row = parse_row(obj)
                if row is None or row.service_date != day:
                    continue
                scan.rows_scanned += 1
                for crs, instant, index in departure_events(row, tiplocs):
                    wanted_key = (day, crs, instant)
                    if wanted_key in wanted:
                        slot = hits.setdefault(wanted_key, {})
                        slot.setdefault(
                            (row.train_uid, row.service_date), Hit(row, index)
                        )
    return scan


def load_jsonl(source: IO[str]) -> Iterator[dict[str, object]]:
    """JSON objects, one per line; a bad line is skipped silently (never echoed)."""
    for line in source:
        if not line.strip():
            continue
        with contextlib.suppress(ValueError):
            obj = json.loads(line)
            if isinstance(obj, dict):
                yield obj


def psql_json(args: argparse.Namespace, sql: str) -> list[dict[str, object]]:
    """Run a read-only query through `kubectl exec` psql; SQL goes on stdin."""
    cmd = [
        "kubectl",
        "--context",
        args.kubectl_context,
        "-n",
        args.namespace,
        "exec",
        "-i",
        args.pg_pod,
        *(["-c", args.pg_container] if args.pg_container else []),
        "--",
        "psql",
        "-X",
        "-At",
        "-v",
        "ON_ERROR_STOP=1",
        "-U",
        args.pg_user,
        "-d",
        args.pg_db,
    ]
    script = (
        f"SET default_transaction_read_only = on;\nBEGIN READ ONLY;\n{sql}\nROLLBACK;\n"
    )
    out = run_text(cmd, stdin=script, env=kube_env())
    return list(load_jsonl(io.StringIO(out)))


def read_inputs(args: argparse.Namespace) -> tuple[list[Candidate], dict[str, str]]:
    """Read the candidates and the TIPLOC map, from files or the database."""
    if args.candidates:
        with Path(args.candidates).open(encoding="utf-8") as handle:
            raw = list(load_jsonl(handle))
    elif args.pg_pod:
        raw = psql_json(args, CANDIDATES_SQL)
    else:
        message = "give --candidates FILE, or --pg-pod to read the database (read-only)"
        raise SystemExit(message)
    candidates = [c for c in (parse_candidate(o) for o in raw) if c is not None]
    tiploc_rows: list[dict[str, object]] = []
    if args.tiploc_map:
        with Path(args.tiploc_map).open(encoding="utf-8") as handle:
            tiploc_rows = list(load_jsonl(handle))
    elif args.pg_pod and not args.candidates:
        tiploc_rows = psql_json(args, TIPLOC_SQL)
    tiplocs = {
        str(r["tiploc"]).upper(): str(r["crs"]).upper()
        for r in tiploc_rows
        if isinstance(r.get("tiploc"), str) and isinstance(r.get("crs"), str)
    }
    return candidates, tiplocs


def make_archive(args: argparse.Namespace, unreadable: list[int]) -> Archive:
    """Open a local mirror or the S3 archive."""
    if args.archive_dir:
        return LocalArchive(Path(args.archive_dir), unreadable)
    conn = load_conn(args.kubectl_context, args.namespace, args.archive_deployment)
    return S3Archive(conn, args.proxy, unreadable)


def n(value: int) -> str:
    """Format a count with thousands separators."""
    return f"{value:,}"


def report(  # noqa: PLR0913, PLR0917  # a flat summary of the run
    candidates: Sequence[Candidate],
    outcomes: Counter[Outcome],
    scan: Scan,
    archive_range: tuple[dt.date, dt.date] | None,
    unreadable: int,
    unchecked: int,
    verbose_ids: dict[Outcome, list[str]] | None,
    *,
    tiploc_map: bool,
) -> None:
    """Print counts only (and, with --verbose, opaque public ids)."""
    span = f"{archive_range[0]} .. {archive_range[1]}" if archive_range else "(none)"
    print(f"archive: {n(scan.objects)} objects, service dates {span}")
    print(
        f"read: {n(scan.dates_read)} service dates, "
        f"{n(scan.rows_scanned)} rows scanned, {n(unreadable)} unreadable lines"
    )
    note = "yes" if tiploc_map else "no (intermediate pins and destinations need it)"
    print(f"tiploc map: {note}")
    print(f"candidates: {n(len(candidates))}")
    for outcome in Outcome:
        print(f"  {outcome.value}: {n(outcomes[outcome])}")
    print(f"  (matched with no destination recorded to check: {n(unchecked)})")
    total = sum(outcomes.values())
    flag = "" if total == len(candidates) else "  MISMATCH WITH CANDIDATES"
    print(f"  total accounted: {n(total)}{flag}")
    if verbose_ids:
        for outcome, labels in verbose_ids.items():
            if outcome is Outcome.SNAPSHOTTED or not labels:
                continue
            print(f"# {outcome.name.lower()} ({len(labels)}): {' '.join(labels)}")


def restore(args: argparse.Namespace) -> int:
    """Match, report and (with --emit-sql) write the SQL files."""
    candidates, tiplocs = read_inputs(args)
    unreadable = [0]
    archive = make_archive(args, unreadable)
    listing = archive.listing()
    archive_range = (min(listing), max(listing)) if listing else None
    wanted = {
        k
        for c in candidates
        if not c.has_snapshot and (k := candidate_key(c)) is not None
    }
    scan = scan_archive(archive, listing, wanted, tiplocs)
    run_at = dt.datetime.now(dt.UTC).replace(microsecond=0)
    outcomes: Counter[Outcome] = Counter()
    labels: dict[Outcome, list[str]] = {o: [] for o in Outcome}
    items: list[tuple[Candidate, dict[str, object]]] = []
    unchecked = 0
    for cand in candidates:
        key = candidate_key(cand)
        found = list(scan.hits.get(key, {}).values()) if key else []
        outcome, hit, check = classify(cand, found, tiplocs, archive_range)
        outcomes[outcome] += 1
        labels[outcome].append(cand.label)
        if outcome is Outcome.MATCHED and hit is not None:
            items.append((cand, build_document(hit.row, cand, run_at)))
            unchecked += check is EndCheck.UNCHECKED
    report(
        candidates,
        outcomes,
        scan,
        archive_range,
        unreadable[0],
        unchecked,
        labels if args.verbose else None,
        tiploc_map=bool(tiplocs),
    )
    if args.emit_sql:
        written, size = emit(
            Path(args.emit_sql), items, outcomes, run_at, args.batch_size, archive_range
        )
        print(
            f"wrote {written} files ({n(size)} bytes, manifest excluded) "
            f"for {n(len(items))} legs to {args.emit_sql}"
        )
    else:
        print("dry run: nothing written (add --emit-sql DIR to write the SQL)")
    return 0


def dump_sql(args: argparse.Namespace) -> int:
    """Print the read-only SQL."""
    which = args.query
    if which in {"candidates", "all"}:
        print("-- candidates: one JSON object per line (run: psql -X -At -f)")
        print(CANDIDATES_SQL)
    if which in {"tiploc", "all"}:
        print("-- tiploc map: one JSON object per line")
        print(TIPLOC_SQL)
    return 0


def parser() -> argparse.ArgumentParser:
    """Build the command line."""
    top = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    sub = top.add_subparsers(dest="command", required=True)
    dump = sub.add_parser("dump-sql", help="print the read-only SQL")
    dump.add_argument("--query", choices=["candidates", "tiploc", "all"], default="all")
    dump.set_defaults(func=dump_sql)
    run = sub.add_parser(
        "restore", help="match legs to the archive (dry run unless --emit-sql)"
    )
    run.add_argument(
        "--candidates",
        help="JSONL from the candidates query (default: read the database)",
    )
    run.add_argument("--tiploc-map", help="JSONL from the tiploc query")
    run.add_argument(
        "--archive-dir", help="a local mirror of the archive instead of S3"
    )
    run.add_argument(
        "--emit-sql",
        metavar="DIR",
        help="write batched SQL, verify, rollback and manifest here",
    )
    run.add_argument(
        "--batch-size",
        type=int,
        default=DEFAULT_BATCH,
        help="legs per transaction (default 200)",
    )
    run.add_argument(
        "--verbose",
        action="store_true",
        help="also print opaque public ids per outcome",
    )
    run.add_argument("--kubectl-context", default="mine-bringer-ts")
    run.add_argument("--namespace", default="distant-signal")
    run.add_argument(
        "--pg-pod", help="Postgres pod to `kubectl exec` psql in (read-only)"
    )
    run.add_argument("--pg-container", default="")
    run.add_argument("--pg-user", default="postgres")
    run.add_argument("--pg-db", default="postgres")
    run.add_argument("--archive-deployment", default="distant-signal-aggregator")
    run.add_argument(
        "--proxy",
        default="socks5h://127.0.0.1:1055",
        help="tailnet SOCKS5 proxy for S3",
    )
    run.set_defaults(func=restore)
    return top


def main(argv: Sequence[str] | None = None) -> int:
    """Entry point."""
    args = parser().parse_args(argv)
    if getattr(args, "batch_size", 1) < 1:
        message = "--batch-size must be at least 1"
        raise SystemExit(message)
    return int(args.func(args))


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit as exc:
        if isinstance(exc.code, str):
            print(exc.code, file=sys.stderr)
            sys.exit(1)
        raise
