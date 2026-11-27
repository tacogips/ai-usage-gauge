use crate::types::PieChartId;
pub struct PieChartSelection;
pub struct RatioTextSelection;
fn ordered(ids: &[PieChartId]) -> Vec<PieChartId> {
    PieChartId::ALL
        .into_iter()
        .filter(|id| ids.contains(id))
        .collect()
}
impl PieChartSelection {
    pub fn normalized(ids: &[PieChartId]) -> Vec<PieChartId> {
        let ids = ordered(ids);
        if ids.is_empty() {
            vec![PieChartId::Budget]
        } else {
            ids
        }
    }
    pub fn toggled(id: PieChartId, ids: &[PieChartId]) -> Vec<PieChartId> {
        let mut ids = Self::normalized(ids);
        if ids.contains(&id) {
            if ids.len() > 1 {
                ids.retain(|v| *v != id);
            }
        } else {
            ids.push(id);
        }
        Self::normalized(&ids)
    }
}
impl RatioTextSelection {
    pub fn normalized(ids: &[PieChartId]) -> Vec<PieChartId> {
        ordered(ids)
    }
    pub fn toggled(id: PieChartId, ids: &[PieChartId]) -> Vec<PieChartId> {
        let mut ids = ordered(ids);
        if ids.contains(&id) {
            ids.retain(|v| *v != id);
        } else {
            ids.push(id);
        }
        ordered(&ids)
    }
}
