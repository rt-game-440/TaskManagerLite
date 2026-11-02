//! 进程页资源列表头上方的整机占用百分比（#96），口径对齐原生任务管理器“进程”页，见 `GLOSSARY.md`“表头总占用”。
//!
//! 只读取每次采样已有的系统级数据（与性能页同源），不新增任何采集。

use tml_core::{Gpu, NetworkAdapter, PhysicalDisk, SystemMetrics};

/// 各资源列的整机占用（0–100）；`None` 表示没有读数，表头显示“—”。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HeaderTotals {
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
    pub disk: Option<f64>,
    pub network: Option<f64>,
    pub gpu: Option<f64>,
}

impl HeaderTotals {
    pub fn compute(
        system: &SystemMetrics,
        disks: &[PhysicalDisk],
        networks: &[NetworkAdapter],
        gpus: &[Gpu],
    ) -> Self {
        let max = |it: &mut dyn Iterator<Item = f64>| it.fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))));
        Self {
            cpu: Some(f64::from(system.cpu_usage)),
            memory: (system.memory_total > 0)
                .then(|| system.memory_used as f64 / system.memory_total as f64 * 100.0),
            // 原生：所有物理磁盘里活动时间最高的那块；没有逐盘读数时退回整机平均值。
            disk: max(&mut disks.iter().filter_map(|d| d.active.map(f64::from)))
                .or_else(|| disks.is_empty().then(|| f64::from(system.disk_active))),
            // 原生：利用率最高的网络适配器（收发合计 / 链路速度）。
            network: max(&mut networks.iter().filter_map(adapter_utilization)),
            // 原生：所有 GPU 中最忙的引擎（`Gpu::usage` 已是该卡各引擎的最大值）。
            gpu: max(&mut gpus.iter().filter_map(|g| g.usage)),
        }
        .clamped()
    }

    fn clamped(mut self) -> Self {
        for v in [&mut self.cpu, &mut self.memory, &mut self.disk, &mut self.network, &mut self.gpu] {
            *v = v.map(|x| if x.is_finite() { x.clamp(0.0, 100.0) } else { 0.0 });
        }
        self
    }
}

fn adapter_utilization(n: &NetworkAdapter) -> Option<f64> {
    if n.link_speed == 0 {
        return None;
    }
    let bytes = match (n.recv, n.send) {
        (None, None) => return None,
        (r, s) => r.unwrap_or(0.0) + s.unwrap_or(0.0),
    };
    Some(bytes * 8.0 / n.link_speed as f64 * 100.0)
}

/// 表头大号数字：与原生一致取整数百分比。
pub fn text(v: Option<f64>) -> String {
    v.map_or_else(|| "—".into(), |v| format!("{:.0}%", v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system(cpu: f32, used: u64, total: u64, disk: f32) -> SystemMetrics {
        SystemMetrics { cpu_usage: cpu, memory_used: used, memory_total: total, disk_active: disk, ..Default::default() }
    }

    fn disk(active: Option<f32>) -> PhysicalDisk {
        PhysicalDisk { active, ..Default::default() }
    }

    fn net(link: u64, recv: Option<f64>, send: Option<f64>) -> NetworkAdapter {
        NetworkAdapter { link_speed: link, recv, send, ..Default::default() }
    }

    fn gpu(usage: Option<f64>) -> Gpu {
        Gpu { usage, ..Default::default() }
    }

    #[test]
    fn cpu_and_memory_come_from_system_metrics() {
        let t = HeaderTotals::compute(&system(12.4, 4, 16, 0.0), &[], &[], &[]);
        assert_eq!(t.cpu, Some(f64::from(12.4f32)));
        assert_eq!(t.memory, Some(25.0));
        assert_eq!(text(t.memory), "25%");
    }

    #[test]
    fn memory_without_total_has_no_reading() {
        let t = HeaderTotals::compute(&system(0.0, 0, 0, 0.0), &[], &[], &[]);
        assert_eq!(t.memory, None);
        assert_eq!(text(t.memory), "—");
    }

    #[test]
    fn disk_takes_the_busiest_physical_disk() {
        let disks = [disk(Some(3.0)), disk(None), disk(Some(87.5))];
        let t = HeaderTotals::compute(&system(0.0, 1, 1, 40.0), &disks, &[], &[]);
        assert_eq!(t.disk, Some(87.5));
    }

    #[test]
    fn disk_falls_back_to_total_only_without_disks() {
        assert_eq!(HeaderTotals::compute(&system(0.0, 1, 1, 40.0), &[], &[], &[]).disk, Some(40.0));
        // 有磁盘但都还没有读数（首次采样）：不拿整机平均值冒充。
        assert_eq!(HeaderTotals::compute(&system(0.0, 1, 1, 40.0), &[disk(None)], &[], &[]).disk, None);
    }

    #[test]
    fn network_takes_the_busiest_adapter_by_link_speed() {
        let nets = [
            // 1 Gbps 上收发合计 12.5 MB/s = 100 Mbps → 10%
            net(1_000_000_000, Some(10_000_000.0), Some(2_500_000.0)),
            // 100 Mbps 上 2.5 MB/s = 20 Mbps → 20%
            net(100_000_000, Some(2_500_000.0), None),
            // 没有链路速度的虚拟适配器不参与
            net(0, Some(1e9), Some(1e9)),
        ];
        let t = HeaderTotals::compute(&system(0.0, 1, 1, 0.0), &[], &nets, &[]);
        assert!((t.network.unwrap() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn network_without_readings_is_none_and_saturates_at_100() {
        assert_eq!(HeaderTotals::compute(&system(0.0, 1, 1, 0.0), &[], &[net(1000, None, None)], &[]).network, None);
        let t = HeaderTotals::compute(&system(0.0, 1, 1, 0.0), &[], &[net(8, Some(10.0), None)], &[]);
        assert_eq!(t.network, Some(100.0));
    }

    #[test]
    fn gpu_takes_the_busiest_adapter() {
        let t = HeaderTotals::compute(&system(0.0, 1, 1, 0.0), &[], &[], &[gpu(Some(5.0)), gpu(None), gpu(Some(61.6))]);
        assert_eq!(t.gpu, Some(61.6));
        assert_eq!(text(t.gpu), "62%");
        assert_eq!(HeaderTotals::compute(&system(0.0, 1, 1, 0.0), &[], &[], &[gpu(None)]).gpu, None);
    }

    #[test]
    fn text_rounds_to_whole_percent() {
        assert_eq!(text(Some(0.2)), "0%");
        assert_eq!(text(Some(99.6)), "100%");
    }
}
