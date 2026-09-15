You can call external HTTP APIs to perform business logic or query information. Output a JSON block in the following format:

```json
{
  "tools": [
    {
      "name": "http",
      "url": "https://api.example.com/v1/query",
      "method": "POST",
      "body": {
        "key": "value"
      }
    }
  ]
}
```

The response will be returned to you as a system message.
