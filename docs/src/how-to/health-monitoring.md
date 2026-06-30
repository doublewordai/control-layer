# Set Up Health Monitoring

> Learn how to monitor the status of AI model endpoints using Control Layer's built-in health monitoring features.

The Control Layer can monitor your endpoints with health checks. Status indicators show whether models are online.

## Status indicators

- **Green dot**: Model is online and responding
- **Red dot**: Model is offline or failing health checks
- **No dot**: Monitoring not configured for this model

## Set up monitoring

From any model's detail page:

1. Click **Configure Monitoring**
2. Choose a monitor type:
   - **Default**: Sends lightweight requests to the model endpoint
   - **Custom HTTP probe**: Check a specific URL path
3. Set the check interval (1 minute, 5 minutes, 15 minutes, or 30 minutes)
4. Click **Save**

The probe starts running immediately after you save.

### Custom HTTP probes

Use a custom HTTP probe when:
- Your endpoint has a dedicated `/health` or `/status` path
- You want to avoid sending inference requests (which may incur costs)
- The default probe doesn't work with your endpoint's authentication

Configure a custom probe:

1. Select **Custom HTTP probe**
2. Enter the path (e.g., `/health`, `/v1/models`)
3. Optionally add custom headers if the health endpoint requires authentication
4. Choose the expected response code (usually 200)

> **Warning**
>
> Default probes send real inference requests to model endpoints. For cost-sensitive endpoints, use a custom HTTP probe pointed at a health endpoint that doesn't incur usage charges.

## Pause and resume monitoring

You can temporarily disable monitoring without deleting your configuration:

1. Go to the model's detail page
2. Click **Monitoring Settings**
3. Toggle **Active** off to pause, or on to resume

Paused probes retain their configuration but don't run checks or affect status indicators.

## View probe statistics

Each monitored model tracks:
- **Uptime percentage**: Successful checks over total checks
- **Last check**: When the probe last ran
- **Response time**: How long the endpoint took to respond
- **Recent history**: Pass/fail for recent checks

Access statistics from the model's detail page under **Monitoring**.

## View uptime history

The Models page includes an uptime toggle (top right) showing historical availability for all monitored models as a timeline visualization.

## Delete a probe

1. Go to the model's detail page
2. Click **Monitoring Settings**
3. Click **Delete Probe**

This removes all monitoring configuration and history for that model.

## Troubleshooting

**False negatives (red dot but model works)**
The monitoring probe may not match the model's actual API. Try a custom HTTP probe pointed at a known-good endpoint.

**Intermittent status**
May indicate:
- Rate limiting from the provider
- Network issues between Control Layer and the endpoint
- Provider instability

Check probe statistics for patterns (e.g., failures at specific times suggest rate limiting).

**Probe never succeeds**
Verify:
1. The endpoint URL is correct
2. Authentication credentials are valid
3. For custom probes, the path exists and returns the expected status code

**High response times**
Response time is measured from the Control Layer to the endpoint. High times may indicate:
- Geographic distance to the provider
- Provider under load
- Network congestion

Consider this baseline when evaluating actual request performance.
