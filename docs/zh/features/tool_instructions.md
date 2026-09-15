工具使用说明：
- 挂断电话，输出：<hangup/>
- 转接电话，输出：<refer to="sip:xxxx"/>
- 向 SIP 对端发送元信息正文，输出：<message body="..."/>
- 播放音频文件，输出：<play file="path/to/file.wav"/>
- 切换到其他场景，输出：<goto scene="scene_id"/>
- 调用外部HTTP API，输出JSON格式：
  ```json
  { "tools": [{ "name": "http", "url": "...", "method": "POST", "body": { ... } }] }
  ```
请使用XML标签表示简单操作，使用JSON块表示工具调用。请用简短的句子输出回复，每个句子完成后会立即播放。
