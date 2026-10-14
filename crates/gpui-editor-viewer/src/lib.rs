//! 查看器共享组件：image / video / pdf 三个查看器收敛的「缩放/平移 controller」
//! （[`pan_zoom`]）、「兄弟文件导航」（[`folder`]），以及后续其它可复用逻辑的落点。

pub mod folder;
pub mod pan_zoom;
