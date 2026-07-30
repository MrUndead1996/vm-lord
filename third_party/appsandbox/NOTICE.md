# AppSandbox legacy backend

VMLord temporarily ships the Windows x64 `appsandbox_core.dll` runtime from
AppSandbox. The backend is loaded dynamically and is an implementation detail
behind the `vmlord-legacy-backend` crate.

Source: `C:\sources\appsandbox\bin\Release\appsandbox_core.dll`

SHA-256: `CEE2F16A2ABA00583F05CBA2321C033F55A18AC933B47502419FE2DBEDDF0E6F`

Only the binary needed by the read-only VM-list shell is included. macOS
components, the AppSandbox desktop application, WebView UI, provisioning tools,
and display resources are not part of this runtime package.
