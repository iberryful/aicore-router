
# claude family

in stream response, input and output tokens can be retrieved from message_stop event.
Here is an example of stream response.
```json
{"type":"message_stop","amazon-bedrock-invocationMetrics":{"inputTokenCount":7,"outputTokenCount":126,"invocationLatency":4069,"firstByteLatency":1117,"cacheReadInputTokenCount":14486,"cacheWriteInputTokenCount":2238}}}
```

# gemini family

in stream response, get token from `usageMetadata`. Input token is `promptTokenCount`. Output token is `totalTokenCount` - `promptTokenCount`.

Here is an example.
```json
{"candidates": [{"content": {"role": "model","parts": [{"text": "example"}]},"finishReason": "STOP"}],"usageMetadata": {"promptTokenCount": 37824,"candidatesTokenCount": 116,"totalTokenCount": 37940,"cachedContentTokenCount": 34745,"trafficType": "ON_DEMAND","promptTokensDetails": [{"modality": "TEXT","tokenCount": 37824}],"cacheTokensDetails": [{"modality": "TEXT","tokenCount": 34745}],"candidatesTokensDetails": [{"modality": "TEXT","tokenCount": 116}]},"modelVersion": "gemini-2.5-pro","createTime": "2025-07-23T02:44:52.672859Z","responseId": "pEyAaNuIKa7DtfAPntqV"}}
```

# openai family

in stream response, get token from `usage`. Input token is `prompt_tokens`, output token is `completion_tokens`.

```json
{"choices":[],"created":1753027923,"id":"chatcmpl-BvQvrsilfomOdCkWu2s8ymR15s3CB","model":"o3-mini-2025-01-31","object":"chat.completion.chunk","system_fingerprint":"fp_e1882df059","usage":{"completion_tokens":21,"completion_tokens_details":{"accepted_prediction_tokens":0,"audio_tokens":0,"reasoning_tokens":0,"rejected_prediction_tokens":0},"prompt_tokens":15,"prompt_tokens_details":{"audio_tokens":0,"cached_tokens":0},"total_tokens":36}}
```
