//! 4KB 数据页，slotted page 布局，与 Node 版逐字节兼容。
//!
//! ```text
//! offset 0      : u8   pageType (1 = data)
//! offset 1..4   : 保留
//! offset 4      : u32  pageNo
//! offset 8      : u16  slotCount
//! offset 10     : u16  freeOffset
//! offset 12     : u32  nextPage（保留）
//! offset 16     : u32  flags（保留）
//! offset 20..24 : 保留
//! offset 24     : slot 目录，每槽 6 字节：offset(u16) length(u16) flags(u8)
//! freeOffset..  : 记录数据区（从页尾向前增长）
//! ```

use crate::error::{DbError, Result};

pub const PAGE_SIZE: usize = 4096;
pub const HEADER_SIZE: usize = 24;
pub const SLOT_SIZE: usize = 6;
pub const FLAG_DELETED: u8 = 0x01;
pub const MAX_RECORD_SIZE: usize = PAGE_SIZE - HEADER_SIZE - SLOT_SIZE * 2;

pub type Page = Vec<u8>;

fn read_u16(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buf[at], buf[at + 1]])
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

fn write_u16(buf: &mut [u8], at: usize, value: u16) {
    let bytes = value.to_le_bytes();
    buf[at] = bytes[0];
    buf[at + 1] = bytes[1];
}

fn write_u32(buf: &mut [u8], at: usize, value: u32) {
    let bytes = value.to_le_bytes();
    buf[at] = bytes[0];
    buf[at + 1] = bytes[1];
    buf[at + 2] = bytes[2];
    buf[at + 3] = bytes[3];
}

pub fn init_page(page_no: u32) -> Page {
    let mut page = vec![0u8; PAGE_SIZE];
    page[0] = 1;
    write_u32(&mut page, 4, page_no);
    write_u16(&mut page, 8, 0);
    write_u16(&mut page, 10, PAGE_SIZE as u16);
    page
}

pub fn slot_count(page: &Page) -> u16 {
    read_u16(page, 8)
}

pub fn free_offset(page: &Page) -> u16 {
    read_u16(page, 10)
}

pub fn free_space(page: &Page) -> i32 {
    free_offset(page) as i32 - (HEADER_SIZE as i32 + slot_count(page) as i32 * SLOT_SIZE as i32)
}

fn slot_pos(slot: u16) -> usize {
    HEADER_SIZE + slot as usize * SLOT_SIZE
}

pub fn is_deleted(page: &Page, slot: u16) -> bool {
    if slot >= slot_count(page) {
        return true;
    }
    let pos = slot_pos(slot);
    page[pos + 4] & FLAG_DELETED != 0
}

/// 插入一条记录，返回 slot 号；空间不足返回 None（与 Node 版语义一致）
pub fn insert_record(page: &mut Page, rec: &[u8]) -> Option<u16> {
    let count = slot_count(page);
    let offset = free_offset(page) as usize;
    let need_end = HEADER_SIZE + (count as usize + 1) * SLOT_SIZE;
    if rec.len() > PAGE_SIZE || offset < rec.len() || offset - rec.len() < need_end {
        return None;
    }
    let rec_offset = offset - rec.len();
    page[rec_offset..offset].copy_from_slice(rec);

    let pos = slot_pos(count);
    write_u16(page, pos, rec_offset as u16);
    write_u16(page, pos + 2, rec.len() as u16);
    page[pos + 4] = 0;

    write_u16(page, 8, count + 1);
    write_u16(page, 10, rec_offset as u16);
    Some(count)
}

pub fn read_record(page: &Page, slot: u16) -> Option<Vec<u8>> {
    if slot >= slot_count(page) {
        return None;
    }
    let pos = slot_pos(slot);
    if page[pos + 4] & FLAG_DELETED != 0 {
        return None;
    }
    let offset = read_u16(page, pos) as usize;
    let len = read_u16(page, pos + 2) as usize;
    if offset + len > PAGE_SIZE {
        return None;
    }
    Some(page[offset..offset + len].to_vec())
}

pub fn delete_record(page: &mut Page, slot: u16) -> bool {
    if slot >= slot_count(page) {
        return false;
    }
    let pos = slot_pos(slot);
    if page[pos + 4] & FLAG_DELETED != 0 {
        return false;
    }
    page[pos + 4] |= FLAG_DELETED;
    true
}

/// 碎片整理：保留 slot 编号（rowId 稳定），只重排活跃记录
pub fn compact_page(page: &mut Page) -> bool {
    let before = free_space(page);
    let count = slot_count(page);

    let mut live: Vec<(u16, Vec<u8>)> = Vec::new();
    for slot in 0..count {
        if let Some(rec) = read_record(page, slot) {
            live.push((slot, rec));
        }
    }

    // 清空 slot 的数据指针，保留删除标记
    for slot in 0..count {
        let pos = slot_pos(slot);
        let flags = page[pos + 4];
        write_u16(page, pos, 0);
        write_u16(page, pos + 2, 0);
        page[pos + 4] = flags & FLAG_DELETED;
    }
    write_u16(page, 10, PAGE_SIZE as u16);

    for (slot, rec) in live {
        let offset = free_offset(page) as usize;
        let rec_offset = offset - rec.len();
        page[rec_offset..offset].copy_from_slice(&rec);
        let pos = slot_pos(slot);
        write_u16(page, pos, rec_offset as u16);
        write_u16(page, pos + 2, rec.len() as u16);
        page[pos + 4] = 0;
        write_u16(page, 10, rec_offset as u16);
    }

    free_space(page) > before
}

pub fn page_no(page: &Page) -> u32 {
    read_u32(page, 4)
}

/// 校验从磁盘读到的页是否属于当前页号，避免读到错位数据
pub fn validate(page: &Page, expected_no: u32) -> Result<()> {
    if page.len() != PAGE_SIZE {
        return Err(DbError::internal(format!(
            "页长度异常: {} 字节（应为 {}）",
            page.len(),
            PAGE_SIZE
        )));
    }
    let actual = page_no(page);
    if actual != expected_no {
        return Err(DbError::internal(format!(
            "页号不匹配: 期望 {} 实际 {}",
            expected_no, actual
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_read_delete() {
        let mut page = init_page(0);
        let a = b"aaaa".to_vec();
        let b = b"bbbbbb".to_vec();

        assert_eq!(insert_record(&mut page, &a), Some(0));
        assert_eq!(insert_record(&mut page, &b), Some(1));
        assert_eq!(slot_count(&page), 2);
        assert_eq!(read_record(&page, 0).unwrap(), a);
        assert_eq!(read_record(&page, 1).unwrap(), b);

        assert!(delete_record(&mut page, 1));
        assert!(read_record(&page, 1).is_none());
        assert!(!delete_record(&mut page, 1));
    }

    #[test]
    fn compact_keeps_slot_numbers() {
        let mut page = init_page(0);
        insert_record(&mut page, b"aaaa");
        insert_record(&mut page, b"bbbbbb");
        insert_record(&mut page, b"cc");
        delete_record(&mut page, 1);

        let before = free_space(&page);
        assert!(compact_page(&mut page));
        assert!(free_space(&page) > before);

        assert_eq!(read_record(&page, 0).unwrap(), b"aaaa".to_vec());
        assert!(read_record(&page, 1).is_none());
        assert_eq!(read_record(&page, 2).unwrap(), b"cc".to_vec());
    }

    #[test]
    fn rejects_when_full() {
        let mut page = init_page(0);
        let big = vec![7u8; 3000];
        assert_eq!(insert_record(&mut page, &big), Some(0));
        assert_eq!(insert_record(&mut page, &big), None);
    }

    #[test]
    fn validate_page_number() {
        let page = init_page(3);
        assert!(validate(&page, 3).is_ok());
        assert!(validate(&page, 4).is_err());
    }
}
