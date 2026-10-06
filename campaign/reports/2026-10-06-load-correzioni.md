# Campagna operativa: fase load

Esito: **SUPERATA**

| voce | valore |
| --- | --- |
| commit | `e8d1ffe79b614a9a9403e492669990f663840236` |
| versione del workspace | 0.2.2 |
| inizio / fine (UTC) | 2026-10-05T21:58:12.196Z / 2026-10-05T22:13:31.132Z |
| ambiente | linux x86_64 (8 CPU), kernel 5.15.0-194-generic, Linux 5.15.0-194-generic x86_64 |
| toolchain / profilo | rustc 1.98.0 (88d9e12ae 2026-08-18) / release |
| seed | 1 |
| durata totale | 15.3 min (funzionale 6.9 s, carico 15.0 min su 15 min pianificati) |
| profilo | 32 worker, obiettivo 40 op/s, max_concurrent_requests 32, requests_per_second 200 |
| operazioni | 36054 (36054 attese, 0 inattese, 0 bloccate) |
| throughput carico | 40.00 op/s (biglietti persi 0) |
| richieste motore / server | 116559 / 112754 (retry 23476) |
| concorrenza e rate osservati | picco 7 su 32, massimo 200 richieste in un secondo |
| soglie | proposta, da approvare: default per il server locale della campagna, non ancora decisi per l'ambiente Plenora |

## Criteri

| criterio | esito | osservato | limite |
| --- | --- | --- | --- |
| nessun panic | superato | 0 | ≤ 0 |
| nessuna operazione bloccata (deadlock) | superato | 0 | ≤ 0 |
| esiti conformi a quelli dichiarati per ogni scenario | superato | 0 | ≤ 0 |
| retry mai oltre max_attempts | superato | 0 | ≤ 0 |
| nessuna amplificazione delle richieste | superato | 0 | ≤ 0 |
| nessun submit duplicato al resume | superato | 0 | ≤ 0 |
| nessun file incompleto pubblicato | superato | 0 | ≤ 0 |
| nessun file parziale lasciato dall'operazione | superato | 0 | ≤ 0 |
| ordine e contenuto dell'enrichment conservati | superato | 0 | ≤ 0 |
| nessun segreto o percorso nei risultati e negli errori | superato | 0 | ≤ 0 |
| remote_effect e retry advice coerenti con il guasto | superato | 0 | ≤ 0 |
| deadline e cancellazione rispettate | superato | 0 | ≤ 0 |
| un Engine chiuso non accetta lavoro | superato | 0 | ≤ 0 |
| una sessione cookie chiusa non raggiunge la rete | superato | 0 | ≤ 0 |
| harness senza errori propri | superato | 0 | ≤ 0 |
| metrics.requests non inferiore alle richieste ricevute dal servizio | superato | 0 | ≤ 0 |
| remote_effect più prudente del necessario (imprecisione) | superato | 1720 osservazioni | non bloccante, da rivedere |
| ogni scenario abilitato eseguito almeno una volta | superato | 34 eseguiti su 34 richiesti, 0 non eseguiti | tutti |
| concorrenza dell'Engine entro max_concurrent_requests | superato | 7 | ≤ 32 |
| richieste al secondo entro requests_per_second | superato | 200 | ≤ 222 |
| p99 di download | superato | 83 | ≤ 10000 |
| p99 di enrich | superato | 779 | ≤ 5000 |
| p99 di job | superato | 377 | ≤ 2000 |
| p99 di ok | superato | 108 | ≤ 1000 |
| p99 di page_offset | superato | 599 | ≤ 2000 |
| p99 di upload | superato | 121 | ≤ 10000 |
| throughput del carico misto rispetto all'obiettivo | superato | 40.00 op/s | ≥ 36.00 op/s |
| misure delle risorse senza errori | superato | 0 | ≤ 0 |
| nessun file temporaneo o parziale a fine campagna | superato | 0 | ≤ 0 |
| picco di file temporanei | superato | 2 | ≤ 256 |
| file temporanei stabilizzati dopo il warm-up | superato | crescita 0, pendenza 1.5/h | crescita ≤ 16 |
| picco di memoria residente (MiB) | superato | 75 | ≤ 512 |
| memoria residente stabilizzata dopo il warm-up (byte) | superato | crescita 14983168, pendenza 96498085.6/h | crescita ≤ 67108864 oppure pendenza ≤ 33554432/h |
| picco di file descriptor | superato | 35 | ≤ 1024 |
| file descriptor stabilizzati dopo il warm-up | superato | crescita -2, pendenza -5.4/h | crescita ≤ 32 oppure pendenza ≤ 16/h |
| descriptor in più a riposo rispetto all'inizio | superato | 0 | ≤ 16 |
| thread stabilizzati dopo il warm-up | superato | crescita 1, pendenza 7.0/h | crescita ≤ 8 |

## Risorse

RSS, file descriptor e thread letti da /proc/self (Linux).

| risorsa | iniziale | finale a riposo | picco | crescita dopo warm-up | pendenza/h |
| --- | --- | --- | --- | --- | --- |
| RSS | 4.7 MiB | 75.3 MiB | 74.8 MiB | 14.3 MiB | 92.0 MiB |
| file descriptor | 7 | 7 | 35 | -2 | -5.44 |
| thread | 9 | 9 | 15 | 1 | 7.03 |
| file temporanei | 0 | 0 | 2 | 0 | 1.45 |

## Latenze per scenario (ms)

| scenario | operazioni | attese | p50 | p95 | p99 | max | errori |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cancel | 688 | 688 | 102 | 103 | 103 | 105 | CANCELLED 688 |
| connect_refused | 694 | 694 | 16 | 98 | 197 | 500 | TRANSPORT_ERROR 694 |
| connect_timeout | 359 | 359 | 3048 | 3113 | 3146 | 3191 | TIMEOUT 359 |
| cookie_session | 1031 | 1031 | 51 | 100 | 123 | 281 | POLICY_VIOLATION 1031 |
| deadline | 716 | 716 | 303 | 303 | 303 | 303 | TIMEOUT 716 |
| deadline_with_control | 1 | 1 | 302 | 302 | 302 | 302 | TIMEOUT 1 |
| dns_failure | 329 | 329 | 37 | 96 | 173 | 306 | DNS_RESOLUTION_FAILED 329 |
| download | 1701 | 1701 | 14 | 59 | 83 | 169 | - |
| download_corrupt | 354 | 354 | 13 | 64 | 102 | 165 | CHECKSUM_MISMATCH 354 |
| download_cut | 302 | 302 | 9 | 44 | 84 | 119 | TRANSPORT_ERROR 302 |
| download_resume | 683 | 683 | 20 | 82 | 144 | 375 | - |
| drop_before_response | 972 | 972 | 16 | 97 | 179 | 356 | TRANSPORT_ERROR 972 |
| engine_churn | 368 | 368 | 1 | 2 | 2 | 3 | ENGINE_CLOSED 368 |
| enrich | 1991 | 1991 | 353 | 599 | 779 | 1463 | - |
| flaky_5xx | 1717 | 1717 | 102 | 193 | 271 | 572 | - |
| job | 1407 | 1407 | 156 | 287 | 377 | 949 | - |
| job_cancel | 350 | 350 | 154 | 224 | 304 | 395 | CANCELLED 350 |
| job_deadline | 342 | 342 | 345 | 369 | 398 | 504 | TIMEOUT 342 |
| job_poll_fault | 672 | 672 | 209 | 336 | 484 | 1177 | - |
| job_resume | 698 | 698 | 189 | 279 | 414 | 786 | POLLING_TIMEOUT 698 |
| ok | 6898 | 6898 | 44 | 75 | 108 | 209 | - |
| page_cursor_fault | 699 | 699 | 226 | 369 | 558 | 829 | HTTP_STATUS 699 |
| page_link | 1329 | 1329 | 242 | 369 | 533 | 854 | - |
| page_offset | 1280 | 1280 | 240 | 373 | 599 | 1061 | - |
| persistent_5xx | 1070 | 1070 | 134 | 222 | 295 | 611 | HTTP_STATUS 1070 |
| post_body_then_drop | 700 | 700 | 2 | 37 | 58 | 140 | TRANSPORT_ERROR 700 |
| rate_limited | 1374 | 1374 | 132 | 2130 | 2196 | 2309 | - |
| runtime_binding | 677 | 677 | 57 | 113 | 183 | 392 | - |
| runtime_deadline | 1 | 1 | 301 | 301 | 301 | 301 | - |
| slow | 2689 | 2689 | 142 | 240 | 267 | 395 | - |
| stall | 664 | 664 | 304 | 345 | 377 | 428 | TIMEOUT 664 |
| tls_failure | 667 | 667 | 17 | 103 | 193 | 272 | TRANSPORT_ERROR 667 |
| truncated | 1012 | 1012 | 17 | 108 | 187 | 486 | TRANSPORT_ERROR 1012 |
| upload | 1619 | 1619 | 57 | 90 | 121 | 196 | - |

## Guasti iniettati e comportamento osservato

| guasto | iniettati | esiti | remote_effect | retry | coerenti / prudenti / incoerenti |
| --- | --- | --- | --- | --- | --- |
| cancellazione | 688 | CANCELLED 688 | unknown 688 | quarantine 688 | 688 / 0 / 0 |
| cancellazione_job | 350 | CANCELLED 350 | unknown 350 | quarantine 350 | 350 / 0 / 0 |
| chiusura_prima_della_risposta | 972 | TRANSPORT_ERROR 972 | unknown 972 | quarantine 972 | 972 / 0 / 0 |
| connect_timeout | 359 | TIMEOUT 359 | unknown 359 | quarantine 359 | 0 / 359 / 0 |
| connessione_rifiutata | 694 | TRANSPORT_ERROR 694 | unknown 694 | quarantine 694 | 0 / 694 / 0 |
| corpo_letto_poi_chiusura | 700 | TRANSPORT_ERROR 700 | unknown 700 | quarantine 700 | 700 / 0 / 0 |
| deadline | 717 | TIMEOUT 717 | unknown 717 | quarantine 717 | 717 / 0 / 0 |
| deadline_job | 342 | TIMEOUT 342 | unknown 342 | quarantine 342 | 342 / 0 / 0 |
| dns_inesistente | 329 | DNS_RESOLUTION_FAILED 329 | none 329 | safe 329 | 329 / 0 / 0 |
| download_corrotto | 354 | CHECKSUM_MISMATCH 354 | unknown 354 | never 354 | 354 / 0 / 0 |
| download_interrotto | 302 | TRANSPORT_ERROR 302 | unknown 302 | quarantine 302 | 302 / 0 / 0 |
| download_interrotto_ripreso | 683 | recuperato 683 | - | - | 683 / 0 / 0 |
| engine_chiuso | 368 | ENGINE_CLOSED 368 | none 368 | never 368 | 368 / 0 / 0 |
| errore_paginazione_persistente | 699 | HTTP_STATUS 699 | unknown 699 | never 699 | 699 / 0 / 0 |
| errore_paginazione_transitorio | 1289 | recuperato 1289 | - | - | 1289 / 0 / 0 |
| errore_polling_transitorio | 672 | recuperato 672 | - | - | 672 / 0 / 0 |
| http_429_retry_after | 1374 | recuperato 1374 | - | - | 1374 / 0 / 0 |
| http_5xx_persistente | 1070 | HTTP_STATUS 1070 | unknown 1070 | never 1070 | 1070 / 0 / 0 |
| http_5xx_transitorio | 1717 | recuperato 1717 | - | - | 1717 / 0 / 0 |
| http_5xx_transitorio_enrichment | 985 | recuperato 985 | - | - | 985 / 0 / 0 |
| polling_timeout_resume | 698 | POLLING_TIMEOUT 698 | unknown 698 | requires_recovery 698 | 698 / 0 / 0 |
| risposta_assente_timeout | 664 | TIMEOUT 664 | unknown 664 | quarantine 664 | 664 / 0 / 0 |
| risposta_troncata | 1012 | TRANSPORT_ERROR 1012 | unknown 1012 | quarantine 1012 | 1012 / 0 / 0 |
| sessione_cookie_chiusa | 1031 | POLICY_VIOLATION 1031 | none 1031 | never 1031 | 1031 / 0 / 0 |
| tls_fallito | 667 | TRANSPORT_ERROR 667 | unknown 667 | quarantine 667 | 0 / 667 / 0 |
