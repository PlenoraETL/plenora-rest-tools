# Campagna operativa: fase smoke

Esito: **SUPERATA**

| voce | valore |
| --- | --- |
| commit | `e8d1ffe79b614a9a9403e492669990f663840236` |
| versione del workspace | 0.2.2 |
| inizio / fine (UTC) | 2026-10-05T21:42:55.261Z / 2026-10-05T21:58:12.099Z |
| ambiente | linux x86_64 (8 CPU), kernel 5.15.0-194-generic, Linux 5.15.0-194-generic x86_64 |
| toolchain / profilo | rustc 1.98.0 (88d9e12ae 2026-08-18) / release |
| seed | 1 |
| durata totale | 15.3 min (funzionale 6.8 s, carico 15.0 min su 15 min pianificati) |
| profilo | 4 worker, obiettivo 4 op/s, max_concurrent_requests 16, requests_per_second non configurato |
| operazioni | 3654 (3654 attese, 0 inattese, 0 bloccate) |
| throughput carico | 4.00 op/s (biglietti persi 0) |
| richieste motore / server | 12131 / 11765 (retry 2403) |
| concorrenza e rate osservati | picco 6 su 16, massimo 59 richieste in un secondo |
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
| remote_effect più prudente del necessario (imprecisione) | superato | 169 osservazioni | non bloccante, da rivedere |
| ogni scenario abilitato eseguito almeno una volta | superato | 34 eseguiti su 34 richiesti, 0 non eseguiti | tutti |
| concorrenza dell'Engine entro max_concurrent_requests | superato | 6 | ≤ 16 |
| richieste al secondo entro requests_per_second | non valutato | 59 | rate non configurato nel profilo |
| p99 di download | superato | 55 | ≤ 10000 |
| p99 di enrich | superato | 381 | ≤ 5000 |
| p99 di job | superato | 236 | ≤ 2000 |
| p99 di ok | superato | 45 | ≤ 1000 |
| p99 di page_offset | superato | 267 | ≤ 2000 |
| p99 di upload | superato | 58 | ≤ 10000 |
| throughput del carico misto rispetto all'obiettivo | superato | 4.00 op/s | ≥ 3.60 op/s |
| misure delle risorse senza errori | superato | 0 | ≤ 0 |
| nessun file temporaneo o parziale a fine campagna | superato | 0 | ≤ 0 |
| picco di file temporanei | superato | 0 | ≤ 256 |
| file temporanei stabilizzati dopo il warm-up | superato | crescita 0, pendenza 0.0/h | crescita ≤ 16 |
| picco di memoria residente (MiB) | superato | 27 | ≤ 512 |
| memoria residente stabilizzata dopo il warm-up (byte) | superato | crescita 5226496, pendenza 31785062.8/h | crescita ≤ 67108864 oppure pendenza ≤ 33554432/h |
| picco di file descriptor | superato | 24 | ≤ 1024 |
| file descriptor stabilizzati dopo il warm-up | superato | crescita -2, pendenza -4.8/h | crescita ≤ 32 oppure pendenza ≤ 16/h |
| descriptor in più a riposo rispetto all'inizio | superato | 0 | ≤ 16 |
| thread stabilizzati dopo il warm-up | superato | crescita 0, pendenza -0.5/h | crescita ≤ 8 |

## Risorse

RSS, file descriptor e thread letti da /proc/self (Linux).

| risorsa | iniziale | finale a riposo | picco | crescita dopo warm-up | pendenza/h |
| --- | --- | --- | --- | --- | --- |
| RSS | 4.8 MiB | 26.3 MiB | 26.5 MiB | 5.0 MiB | 30.3 MiB |
| file descriptor | 7 | 7 | 24 | -2 | -4.78 |
| thread | 9 | 9 | 12 | 0 | -0.55 |
| file temporanei | 0 | 0 | 0 | 0 | 0.00 |

## Latenze per scenario (ms)

| scenario | operazioni | attese | p50 | p95 | p99 | max | errori |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cancel | 59 | 59 | 103 | 103 | 103 | 103 | CANCELLED 59 |
| connect_refused | 62 | 62 | 3 | 5 | 6 | 6 | TRANSPORT_ERROR 62 |
| connect_timeout | 35 | 35 | 3009 | 3009 | 3009 | 3009 | TIMEOUT 35 |
| cookie_session | 99 | 99 | 44 | 46 | 50 | 50 | POLICY_VIOLATION 99 |
| deadline | 68 | 68 | 303 | 303 | 303 | 303 | TIMEOUT 68 |
| deadline_with_control | 1 | 1 | 301 | 301 | 301 | 301 | TIMEOUT 1 |
| dns_failure | 45 | 45 | 37 | 100 | 126 | 126 | DNS_RESOLUTION_FAILED 45 |
| download | 158 | 158 | 7 | 48 | 55 | 62 | - |
| download_corrupt | 29 | 29 | 4 | 44 | 49 | 49 | CHECKSUM_MISMATCH 29 |
| download_cut | 33 | 33 | 4 | 8 | 12 | 12 | TRANSPORT_ERROR 33 |
| download_resume | 63 | 63 | 7 | 19 | 53 | 53 | - |
| drop_before_response | 109 | 109 | 4 | 6 | 7 | 7 | TRANSPORT_ERROR 109 |
| engine_churn | 66 | 66 | 1 | 2 | 3 | 3 | ENGINE_CLOSED 66 |
| enrich | 216 | 216 | 271 | 357 | 381 | 402 | - |
| flaky_5xx | 163 | 163 | 87 | 92 | 132 | 132 | - |
| job | 145 | 145 | 101 | 197 | 236 | 236 | - |
| job_cancel | 40 | 40 | 109 | 110 | 110 | 110 | CANCELLED 40 |
| job_deadline | 42 | 42 | 304 | 345 | 345 | 345 | TIMEOUT 42 |
| job_poll_fault | 66 | 66 | 189 | 240 | 280 | 280 | - |
| job_resume | 69 | 69 | 148 | 189 | 189 | 189 | POLLING_TIMEOUT 69 |
| ok | 669 | 669 | 1 | 44 | 45 | 45 | - |
| page_cursor_fault | 68 | 68 | 177 | 267 | 307 | 307 | HTTP_STATUS 68 |
| page_link | 136 | 136 | 220 | 267 | 267 | 272 | - |
| page_offset | 145 | 145 | 220 | 263 | 267 | 268 | - |
| persistent_5xx | 91 | 91 | 90 | 132 | 133 | 133 | HTTP_STATUS 91 |
| post_body_then_drop | 88 | 88 | 1 | 2 | 2 | 2 | TRANSPORT_ERROR 88 |
| rate_limited | 126 | 126 | 89 | 2016 | 2065 | 2092 | - |
| runtime_binding | 70 | 70 | 14 | 90 | 92 | 92 | - |
| runtime_deadline | 1 | 1 | 302 | 302 | 302 | 302 | - |
| slow | 280 | 280 | 92 | 193 | 222 | 244 | - |
| stall | 59 | 59 | 302 | 302 | 302 | 302 | TIMEOUT 59 |
| tls_failure | 72 | 72 | 4 | 7 | 7 | 7 | TRANSPORT_ERROR 72 |
| truncated | 113 | 113 | 4 | 7 | 7 | 8 | TRANSPORT_ERROR 113 |
| upload | 168 | 168 | 9 | 48 | 58 | 62 | - |

## Guasti iniettati e comportamento osservato

| guasto | iniettati | esiti | remote_effect | retry | coerenti / prudenti / incoerenti |
| --- | --- | --- | --- | --- | --- |
| cancellazione | 59 | CANCELLED 59 | unknown 59 | quarantine 59 | 59 / 0 / 0 |
| cancellazione_job | 40 | CANCELLED 40 | unknown 40 | quarantine 40 | 40 / 0 / 0 |
| chiusura_prima_della_risposta | 109 | TRANSPORT_ERROR 109 | unknown 109 | quarantine 109 | 109 / 0 / 0 |
| connect_timeout | 35 | TIMEOUT 35 | unknown 35 | quarantine 35 | 0 / 35 / 0 |
| connessione_rifiutata | 62 | TRANSPORT_ERROR 62 | unknown 62 | quarantine 62 | 0 / 62 / 0 |
| corpo_letto_poi_chiusura | 88 | TRANSPORT_ERROR 88 | unknown 88 | quarantine 88 | 88 / 0 / 0 |
| deadline | 69 | TIMEOUT 69 | unknown 69 | quarantine 69 | 69 / 0 / 0 |
| deadline_job | 42 | TIMEOUT 42 | unknown 42 | quarantine 42 | 42 / 0 / 0 |
| dns_inesistente | 45 | DNS_RESOLUTION_FAILED 45 | none 45 | safe 45 | 45 / 0 / 0 |
| download_corrotto | 29 | CHECKSUM_MISMATCH 29 | unknown 29 | never 29 | 29 / 0 / 0 |
| download_interrotto | 33 | TRANSPORT_ERROR 33 | unknown 33 | quarantine 33 | 33 / 0 / 0 |
| download_interrotto_ripreso | 63 | recuperato 63 | - | - | 63 / 0 / 0 |
| engine_chiuso | 66 | ENGINE_CLOSED 66 | none 66 | never 66 | 66 / 0 / 0 |
| errore_paginazione_persistente | 68 | HTTP_STATUS 68 | unknown 68 | never 68 | 68 / 0 / 0 |
| errore_paginazione_transitorio | 146 | recuperato 146 | - | - | 146 / 0 / 0 |
| errore_polling_transitorio | 66 | recuperato 66 | - | - | 66 / 0 / 0 |
| http_429_retry_after | 126 | recuperato 126 | - | - | 126 / 0 / 0 |
| http_5xx_persistente | 91 | HTTP_STATUS 91 | unknown 91 | never 91 | 91 / 0 / 0 |
| http_5xx_transitorio | 163 | recuperato 163 | - | - | 163 / 0 / 0 |
| http_5xx_transitorio_enrichment | 110 | recuperato 110 | - | - | 110 / 0 / 0 |
| polling_timeout_resume | 69 | POLLING_TIMEOUT 69 | unknown 69 | requires_recovery 69 | 69 / 0 / 0 |
| risposta_assente_timeout | 59 | TIMEOUT 59 | unknown 59 | quarantine 59 | 59 / 0 / 0 |
| risposta_troncata | 113 | TRANSPORT_ERROR 113 | unknown 113 | quarantine 113 | 113 / 0 / 0 |
| sessione_cookie_chiusa | 99 | POLICY_VIOLATION 99 | none 99 | never 99 | 99 / 0 / 0 |
| tls_fallito | 72 | TRANSPORT_ERROR 72 | unknown 72 | quarantine 72 | 0 / 72 / 0 |
