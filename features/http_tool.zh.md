你可以通过调用外部 HTTP API 来执行业务逻辑或查询信息。请输出以下格式的 JSON 块：

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

调用结果将以系统消息的形式返回给你。
