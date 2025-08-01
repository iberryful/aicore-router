# SAP AI Core API to get deployments

## Request

```shell
curl --request GET \
  --url <aicore_api_url>/v2/lm/deployments \
  --header 'ai-resource-group: <resource group>' \
  --header 'Authorization: Bearer <token>'
```

## Response
```json
{
  "count": 3,
  "resources": [
    {
      "id": "xxxx",
      "createdAt": "2025-07-28T22:45:25Z",
      "modifiedAt": "2025-07-31T12:54:13Z",
      "status": "RUNNING",
      "details": {
        "resources": {
          "backendDetails": {
            "model": {
              "name": "gpt-4o",
              "version": "latest"
            }
          },
          "backend_details": {
            "model": {
              "name": "gpt-4o",
              "version": "latest"
            }
          }
        },
        "scaling": {
          "backendDetails": {},
          "backend_details": {}
        }
      },
      "scenarioId": "foundation-models",
      "configurationId": "xxxxx",
      "latestRunningConfigurationId": "xxx",
      "lastOperation": "CREATE",
      "targetStatus": "RUNNING",
      "submissionTime": "2025-07-28T22:45:59Z",
      "startTime": "2025-07-28T22:47:02Z",
      "configurationName": "xxx",
      "deploymentUrl": "xxxxx"
    },
    {
      "id": "xxxx",
      "createdAt": "2025-07-26T14:39:25Z",
      "modifiedAt": "2025-07-31T12:54:13Z",
      "status": "RUNNING",
      "details": {
        "resources": {
          "backendDetails": {
            "model": {
              "name": "text-embedding-3-large",
              "version": "latest"
            }
          },
          "backend_details": {
            "model": {
              "name": "text-embedding-3-large",
              "version": "latest"
            }
          }
        },
        "scaling": {
          "backendDetails": {},
          "backend_details": {}
        }
      },
      "scenarioId": "foundation-models",
      "configurationId": "xxxx",
      "latestRunningConfigurationId": "xxxx",
      "lastOperation": "CREATE",
      "targetStatus": "RUNNING",
      "submissionTime": "2025-07-26T14:41:12Z",
      "startTime": "2025-07-26T14:43:51Z",
      "configurationName": "text-embedding-3-large",
      "deploymentUrl": "xxxx"
    },
    {
      "id": "xxxx",
      "createdAt": "2025-07-26T14:38:16Z",
      "modifiedAt": "2025-07-31T12:54:12Z",
      "status": "RUNNING",
      "details": {
        "resources": {
          "backendDetails": {
            "model": {
              "name": "o1",
              "version": "latest"
            }
          },
          "backend_details": {
            "model": {
              "name": "o1",
              "version": "latest"
            }
          }
        },
        "scaling": {
          "backendDetails": {},
          "backend_details": {}
        }
      },
      "scenarioId": "foundation-models",
      "configurationId": "xxxx",
      "latestRunningConfigurationId": "xxxxxx",
      "lastOperation": "CREATE",
      "targetStatus": "RUNNING",
      "submissionTime": "2025-07-26T14:41:09Z",
      "startTime": "2025-07-26T14:43:51Z",
      "configurationName": "o1",
      "deploymentUrl": "xxxxx"
    }
  ]
}
```

## How to parse response

- each resource is a deployment.
- we want to extract status, configurationName, modelName, modelVersion,id,startTime
