#!/usr/bin/env python3
"""Scrive i corpus iniziali versionati dei target di fuzz in fuzz/corpus/.

Deterministico: stessi file, stessi byte a ogni esecuzione. I semi sono
pochi e piccoli, uno per forma significativa dell'input; il resto lo trova
libFuzzer. Si rilancia dopo aver cambiato un formato d'ingresso di un
target:

    python fuzz/genera_corpus.py

Il formato d'ingresso di ogni target è descritto in fuzz/README.md.
"""

from __future__ import annotations

import json
import shutil
from pathlib import Path

CORPUS = Path(__file__).resolve().parent / "corpus"

MESSAGE_ID = "11111111-1111-4111-8111-111111111111"
CORRELATION_ID = "22222222-2222-4222-8222-222222222222"


def compact(value: object) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True).encode()


def response_body() -> dict[str, bytes]:
    # Primo byte: formato (0 JSON, 1 NDJSON, 2 CSV, 3 XML, 4 testo, 5 binario);
    # secondo byte: delimitatore CSV; poi il body.
    return {
        "json_annidato": b"\x00," + compact(
            {"data": {"items": [{"id": 1, "name": "Ada"}, {"id": 2.5, "name": None}]}, "next": "/p?c=2"}
        ),
        "json_non_valido": b'\x00,{"token": "abc" oops}',
        "ndjson_righe_vuote": b'\x01,{"id":1}\n\n  {"id":2}\r\n[1,"a"]\n',
        "csv_virgole_quotate": b'\x02,city,pop\nRoma,"2,873,000"\n"New\nYork","8 ""M"""\n',
        "csv_punto_e_virgola": b"\x02;a;b\n1;2\n3;4\n",
        "csv_tab_larghezza_diversa": b"\x02\ta\tb\n1\t2\t3\n",
        "xml_entita_cdata": (
            b'\x03,<?xml version="1.0"?>\n<r:root xmlns:r="u" id="1">'
            b"<item>Fish &amp; Chips</item><item><![CDATA[ <x> ]]></item>"
            b'<empty a="&#x9;b"/> testo </r:root>'
        ),
        "xml_dtd": b'\x03,<!DOCTYPE a [<!ENTITY e "x">]><a>&e;</a>',
        "testo_utf8": "\x04,ciao è €".encode(),
        "binario": b"\x05,\x00\xff\x01\x80",
    }


def json_path() -> dict[str, bytes]:
    document = compact(
        {
            "data": {
                "items": [
                    {"name": "Ada", "kind": "person", "id": 1},
                    {"name": "Grace", "kind": "admin", "id": "2"},
                ],
                "a.b": {"x y": True},
            },
            "cursor": None,
        }
    )
    return {
        "punti_e_indice": b"$.data.items[0].name\n" + document,
        "parentesi_e_negativo": b"data['items'][-1]\n" + document,
        "filtro_quotato": b"data.items[kind='admin'].name\n" + document,
        "filtro_numerico": b"data.items[id=1]\n" + document,
        "chiave_con_punto": b'data["a.b"]["x y"]\n' + document,
        "radice": b"$\n" + document,
    }


def link_header() -> dict[str, bytes]:
    # Prima riga: relazione cercata; poi il valore dell'header Link.
    return {
        "next_e_last": b'next\n<https://api.example.com/items?page=2>; rel="next", '
        b'<https://api.example.com/items?page=9>; rel="last"',
        "virgole_nel_target": b'NEXT\n<https://x.test/i?c=a,b>; rel="next alternate"; title="a, b"',
        "relazione_uri": b'http://example.com/rel\n</a>; rel="http://example.com/rel"',
        "escape_e_rel_doppio": b'next\n</p>; rel="pr\\ev"; rel=next, </n>; rel=next',
        "non_ben_formato": b'next\n<https://x.test/a; rel="next',
    }


def remote_headers() -> dict[str, bytes]:
    # Primo byte: header (0 Retry-After, 1 Content-Range, 2 ETag,
    # 3 Cache-Control, 4 Set-Cookie); secondo byte: lunghezza minima dei
    # Set-Cookie in multipli di 64 byte; poi il valore.
    return {
        "retry_after_secondi": b"\x00\x00120",
        "retry_after_enorme": b"\x00\x0099999999999999999999999",
        "retry_after_data": b"\x00\x00Wed, 21 Oct 2015 07:28:00 GMT",
        "content_range": b"\x01\x00bytes 0-99/1000",
        "content_range_incoerente": b"\x01\x00bytes 10-5/4",
        "etag_forte": b'\x02\x00 "abc" ',
        "etag_debole": b'\x02\x00W/"x"',
        "cache_control": b'\x03\x00max-age="60", no-cache, private',
        "set_cookie_brevi": b"\x04\x00id=1; Path=/; Secure\nsid=2; Domain=example.com",
        "set_cookie_oltre_limite": b"\x04\x82big=",
    }


def execution_request_payloads() -> dict[str, dict]:
    return {
        "test_get": {
            "schema_version": 1,
            "operation": "test",
            "connection": {"url": "https://api.example.com/items/{id}", "method": "GET"},
            "input": {"params": {"id": 7}, "records": []},
        },
        "generate_cursore": {
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": "https://api.example.com/items",
                "headers": {"Accept": "application/json"},
                "parameters": [{"name": "q", "location": "query"}],
                "response": {"format": "json", "records_path": "data.items"},
                "pagination": {"type": "cursor", "cursor_param": "c", "cursor_path": "next"},
                "retry": {"max_attempts": 2},
            },
            "input": {"params": {"q": "roma"}},
            "options": {"idempotency_key": "chiave-1"},
        },
        "enrich_csv": {
            "schema_version": 1,
            "operation": "enrich",
            "connection": {
                "url": "https://api.example.com/geo",
                "method": "POST",
                "request": {"body_type": "json"},
                "response": {"format": "csv", "delimiter": ";"},
                "parameters": [{"name": "city", "location": "body"}],
            },
            "input": {"records": [{"city": "Roma"}, {"city": "Milano"}]},
            "options": {"enrichment_concurrency": 2},
        },
        "polling_oauth": {
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": "https://api.example.com/jobs",
                "method": "POST",
                "auth": {
                    "type": "oauth2_client_credentials",
                    "token_url": "https://auth.example.com/token",
                    "client_id": "id",
                    "client_secret": "segreto-del-cliente",
                },
                "polling": {
                    "url_template": "/jobs/{job_id}",
                    "id_path": "id",
                    "status_path": "status",
                    "pending_values": ["running"],
                    "success_values": ["done"],
                    "failure_values": ["failed"],
                },
            },
        },
        "download": {
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": "https://files.example.com/a.bin"},
            "input": {"file": {"path": "a.bin", "overwrite": True}},
        },
    }


def execution_request() -> dict[str, bytes]:
    seeds = {name: compact(payload) for name, payload in execution_request_payloads().items()}
    seeds["campo_sconosciuto"] = b'{"schema_version":1,"operation":"test","connection":{"url":"x","nope":1}}'
    return seeds


def runtime_message() -> dict[str, bytes]:
    def envelope(operation: str, contract: str, payload: dict) -> bytes:
        return compact(
            {
                "schema_version": 1,
                "contract": "plenora-runtime-binding-v1",
                "kind": "request",
                "content_type": "application/json",
                "metadata": {
                    "plenora.message.id": MESSAGE_ID,
                    "plenora.trace.correlation_id": CORRELATION_ID,
                    "plenora.capability.name": "plenora.rest-tools",
                    "plenora.capability.version": "1",
                    "plenora.capability.operation": operation,
                    "plenora.operation.version": "1",
                    "plenora.input.contract": contract,
                },
                "payload": payload,
            }
        )

    payloads = execution_request_payloads()
    request_contract = "plenora-rest-execution-request-v1"
    test = dict(payloads["test_get"])
    test["connection"] = dict(test["connection"], credential_ref="vault://tenant/api")
    return {
        "test": envelope("rest.test", request_contract, test),
        "generate": envelope("rest.generate", request_contract, payloads["generate_cursore"]),
        "enrich": envelope("rest.enrich", request_contract, payloads["enrich_csv"]),
        "download_artifact": envelope(
            "rest.download",
            "plenora-rest-file-transfer-input-v1",
            {
                "schema_version": 1,
                "operation": "download",
                "connection": {"url": "https://files.example.com/a.bin"},
                "input": {"file": {"artifact_sink": {"reference": "artifact://tenant/out"}}},
            },
        ),
        "selettore_diverso": envelope("rest.generate", request_contract, payloads["test_get"]),
        "non_json": b'{"schema_version":1,"contract":',
    }


def main() -> None:
    targets = {
        "response_body": response_body(),
        "json_path": json_path(),
        "link_header": link_header(),
        "remote_headers": remote_headers(),
        "execution_request": execution_request(),
        "runtime_message": runtime_message(),
    }
    for target, seeds in targets.items():
        directory = CORPUS / target
        if directory.exists():
            shutil.rmtree(directory)
        directory.mkdir(parents=True)
        for name, data in sorted(seeds.items()):
            (directory / name).write_bytes(data)


if __name__ == "__main__":
    main()
