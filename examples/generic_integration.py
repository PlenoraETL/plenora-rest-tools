from plenora_rest import Engine


engine = Engine()

connection = {
    "url": "https://api.example.com/weather",
    "method": "GET",
    "auth": {"type": "none"},
    "static_parameters": {"current_weather": "true"},
    "parameters": [
        {
            "name": "latitude",
            "mode": "mapped",
            "source": "lat",
            "required": True,
        },
        {
            "name": "longitude",
            "mode": "mapped",
            "source": "lon",
            "required": True,
        },
    ],
    "response": {
        "output_mapping": [
            {
                "path": "current.temperature",
                "column": "temperature",
            }
        ]
    },
    "retry": {"max_attempts": 3},
    "requests_per_second": 10,
}

result = engine.enrich(
    connection,
    [
        {"lat": 41.9028, "lon": 12.4964, "city": "Rome"},
        {"lat": 45.4642, "lon": 9.1900, "city": "Milan"},
    ],
)

if result["status"] == "failed":
    raise RuntimeError(result["errors"])

print(result["output"]["records"])
