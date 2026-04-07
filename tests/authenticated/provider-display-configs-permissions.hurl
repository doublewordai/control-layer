# Standard users should be able to read provider display configs for model icons
GET http://localhost:3001/admin/api/v1/provider-display-configs
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
