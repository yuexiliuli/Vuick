// 内存只涨不降修复的验收：`math_slots` / `code_parse` 的「容量上限 + LRU」与
// `install_open` 换源清空（同一标签页反复打开不同公式/代码文档时位图 + GPU 纹理
// 只涨不降的根因）。纯单元测（缓存自身语义）+ 换源集成测（装配路径清空）。
use super::super::{CodeParseCache, CodeParseState, MathSlot, MathSlotCache};

/// ①（单元）容量上限 + LRU：插入 > cap 个公式后驻留数 ≤ 上限，且被淘汰的是
/// 最旧插入（最久未用）项。
#[test]
fn math_slots_cache_cap_evicts_oldest() {
    let mut c = MathSlotCache::new(3);
    let k = |i: &str| gpui::SharedString::from(i);
    let _ = c.insert(k("k0"), MathSlot::Loading);
    let _ = c.insert(k("k1"), MathSlot::Loading);
    let _ = c.insert(k("k2"), MathSlot::Loading);
    assert_eq!(c.len(), 3);
    let write = c.insert(k("k3"), MathSlot::Loading);
    assert_eq!(c.len(), 3, "超上限后驻留数须 ≤ 上限（k0 被淘汰）");
    assert!(
        write.evicted.is_some(),
        "触发淘汰时应回传被淘汰槽（供调用方 drop_image）"
    );
    assert!(c.get("k0").is_none(), "最旧插入 k0 应被淘汰");
    for key in ["k1", "k2", "k3"] {
        assert!(c.get(key).is_some(), "新近插入 {key} 应驻留");
    }
}

/// ①（单元）LRU 的「最近使用」语义：touch 过的键受保护，淘汰的是未触摸的最旧项。
#[test]
fn math_slots_cache_touch_protects_recent() {
    let mut c = MathSlotCache::new(3);
    let k = |i: &str| gpui::SharedString::from(i);
    let _ = c.insert(k("A"), MathSlot::Loading);
    let _ = c.insert(k("B"), MathSlot::Loading);
    let _ = c.insert(k("C"), MathSlot::Loading);
    c.touch("A"); // A 由最旧 → 最近使用
    let _ = c.insert(k("D"), MathSlot::Loading);
    assert_eq!(c.len(), 3);
    assert!(c.get("A").is_some(), "touch 过的 A 应受保护（最近使用）");
    assert!(c.get("B").is_none(), "未触摸的最旧 B 应被淘汰");
    assert!(c.get("C").is_some() && c.get("D").is_some());
}

/// ③（单元）`code_parse` 同理：只测容量上限 + LRU 最旧淘汰（runs 是纯 CPU 数据）。
#[test]
fn code_parse_cache_cap_evicts_oldest() {
    let mut c = CodeParseCache::new(2);
    let k = |i: &str| {
        (
            gpui::SharedString::from(i),
            gpui::SharedString::from("fn f(){}"),
        )
    };
    let _ = c.insert(k("k0"), CodeParseState::Pending);
    let _ = c.insert(k("k1"), CodeParseState::Pending);
    assert_eq!(c.len(), 2);
    let write = c.insert(k("k2"), CodeParseState::Pending);
    assert_eq!(c.len(), 2, "超过上限后驻留数须 ≤ 上限（k0 被淘汰）");
    assert!(write.evicted.is_some());
    assert!(!c.contains_key(&k("k0")), "最旧插入 k0 应被淘汰");
    assert!(c.contains_key(&k("k1")) && c.contains_key(&k("k2")));
}

/// ②（集成）`install_open` 换源后：`math_slots` / `code_parse` 清空，被清空的
/// `Ready` 位图纹理收进 `pending_math_drop`（下一帧 `sync_math` drop_image 释放），
/// 在途待办队列一并作废。
#[gpui::test]
fn install_open_clears_math_and_code_caches(cx: &mut TestAppContext) {
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("$$\na = b\n$$\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let buf = image::RgbaImage::from_raw(8, 8, vec![0u8; 8 * 8 * 4]).unwrap();
        let img = std::sync::Arc::new(gpui::RenderImage::new(vec![image::Frame::new(buf)]));
        // 换源前：math_slots 有一个 Ready + 一个 Loading；code_parse 有一条待办缓存。
        {
            let cache = ed.read(cx).math_slots();
            let mut slots = cache.borrow_mut();
            slots.insert(
                gpui::SharedString::from("c1"),
                MathSlot::Ready(img.clone(), 20., 24.),
            );
            slots.insert(gpui::SharedString::from("c2"), MathSlot::Loading);
            assert_eq!(slots.len(), 2);
        }
        {
            let mut m = ed.read(cx).code_parse.lock().unwrap();
            m.insert(
                (
                    gpui::SharedString::from("rust"),
                    gpui::SharedString::from("fn main(){}"),
                ),
                CodeParseState::Pending,
            );
            assert_eq!(m.len(), 1);
        }
        // 换源：整篇装配（install_open 语义 = 打开新文档，旧文档缓存整批作废）。
        let r = WyEditor::open_from_str(gpui::SharedString::from("全新文档\n"));
        ed.update(cx, |e, _| {
            e.install_open(r);
        });
        assert_eq!(
            ed.read(cx).math_slots().borrow().len(),
            0,
            "换源后 math_slots 应清空"
        );
        {
            let m = ed.read(cx).code_parse.lock().unwrap();
            assert_eq!(m.len(), 0, "换源后 code_parse 应清空");
        }
        // Ready 位图纹理应收进 pending_math_drop（Loading 位图未生成、不入列）。
        {
            let pending = ed.read(cx).pending_math_drop.borrow();
            assert_eq!(pending.len(), 1, "被清空的 Ready 位图应收敛待释放");
        }
        // 换源代已自增（在途后台任务完成回来自检会放弃写入新文档槽）。
        let m0 = ed.read(cx).math_epoch.get();
        let c0 = ed.read(cx).code_epoch.get();
        let r2 = WyEditor::open_from_str(gpui::SharedString::from("再换一篇\n"));
        ed.update(cx, |e, _| {
            e.install_open(r2);
        });
        assert!(ed.read(cx).math_epoch.get() > m0, "每次换源 math_epoch 应自增");
        assert!(ed.read(cx).code_epoch.get() > c0, "每次换源 code_epoch 应自增");
    });
}
