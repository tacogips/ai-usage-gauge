pub mod api_client;
pub mod assets;
pub mod desktop_executable;
pub mod http_server;
mod machine_routes;
pub mod query;
pub mod router;

pub use api_client::{ApiClientError, DashboardAPIClient};
pub use assets::{AssetResponse, StaticAssetResolver};
pub use desktop_executable::{resolve_desktop_executable, DesktopExecutableError};
pub use http_server::{DashboardHTTPServer, ServerHandle};
pub use query::{
    BudgetApiResponse, CostSeriesPoint, CostSeriesResponse, DashboardQueryService, DirectoryFilter,
    MetricsResponse, PointsResponse,
};
pub use router::{MachineDashboardRouter, RouterRequest, RouterResponse};
