use core::mem::size_of;
use core::slice::from_raw_parts;
use core::str::from_utf8;

use super::common::*;
use super::dtb_format::*;
use super::struct_item::*;

#[derive(Clone, Debug)]
pub struct DtbStructIterator<'a> {
    struct_block: &'a [u8],
    strings_block: &'a [u8],
    offset: usize,
}

const DTB_TOKEN_SIZE: usize = 4;

impl<'a> DtbStructIterator<'a> {
    fn set_offset(&mut self, offset: usize) {
        self.offset =
            ((offset + DTB_TOKEN_SIZE - 1) / DTB_TOKEN_SIZE) * DTB_TOKEN_SIZE;
    }

    fn read_begin_node(&mut self) -> Result<StructItem<'a>> {
        let offset = self.offset + DTB_TOKEN_SIZE;
        for (i, chr) in (&self.struct_block[offset..]).iter().enumerate() {
            if *chr != 0 {
                continue;
            }
            return match from_utf8(&self.struct_block[offset..offset + i]) {
                Ok(name) => {
                    self.set_offset(offset + i + 1);
                    Ok(StructItem::BeginNode { name: name })
                }
                Err(err) => Err(Error::BadStrEncoding(err)),
            };
        }
        Err(Error::BadNodeName)
    }

    fn assert_enough_struct(&self, offset: usize, size: usize) -> Result<()> {
        if self.struct_block.len() < offset + size {
            Err(Error::UnexpectedEndOfStruct)
        } else {
            Ok(())
        }
    }

    fn read_property(&mut self) -> Result<StructItem<'a>> {
        let mut offset = self.offset + DTB_TOKEN_SIZE;
        let desc_size = size_of::<DtbPropertyDesc>();
        self.assert_enough_struct(offset, desc_size)?;

        let desc_be = unsafe {
            &*((&self.struct_block[offset..]).as_ptr()
                as *const DtbPropertyDesc) as &DtbPropertyDesc
        };
        offset += desc_size;

        let value_size = u32::from_be(desc_be.value_size) as usize;
        self.assert_enough_struct(offset, value_size)?;
        let value = &self.struct_block[offset..offset + value_size];
        offset += value_size;

        let name_offset = u32::from_be(desc_be.name_offset) as usize;
        for (i, chr) in (&self.strings_block[name_offset..]).iter().enumerate()
        {
            if *chr != 0 {
                continue;
            }
            return match from_utf8(
                &self.strings_block[name_offset..name_offset + i],
            ) {
                Ok(name) => {
                    self.set_offset(offset);
                    Ok(StructItem::Property {
                        name: name,
                        value: value,
                    })
                }
                Err(err) => Err(Error::BadStrEncoding(err)),
            };
        }

        Err(Error::BadPropertyName)
    }

    pub fn next(&mut self) -> Result<StructItem<'a>> {
        loop {
            self.assert_enough_struct(self.offset, DTB_TOKEN_SIZE)?;

            let token = u32::from_be(unsafe {
                *((&self.struct_block[self.offset..]).as_ptr() as *const u32)
            });

            if token == DTB_NOP {
                self.offset += DTB_TOKEN_SIZE;
                continue;
            }

            return match token {
                DTB_BEGIN_NODE => self.read_begin_node(),
                DTB_PROPERTY => self.read_property(),
                DTB_END_NODE => {
                    self.offset += DTB_TOKEN_SIZE;
                    Ok(StructItem::EndNode)
                }
                DTB_END => Err(Error::NoMoreStructItems),
                _ => Err(Error::BadStructToken),
            };
        }
    }

    pub fn find(
        &self,
        path: &str,
    ) -> Result<(StructItem<'a>, DtbStructIterator<'a>)> {
        let path = if path.ends_with("/") {
            &path[..path.len() - 1]
        } else {
            path
        };

        let mut item = StructItem::EndNode;
        let mut iter = self.clone();

        for part in path.split("/") {
            let mut level = 0;
            loop {
                item = iter.next()?;
                match item {
                    StructItem::BeginNode { name } => {
                        if level == 0 && name == part {
                            break;
                        }
                        level += 1;
                    }
                    StructItem::Property { name, value: _ } => {
                        if level == 0 && name == part {
                            break;
                        }
                    }
                    StructItem::EndNode => level -= 1,
                }
            }
        }

        Ok((item, iter))
    }
}

impl<'a> Iterator for DtbStructIterator<'a> {
    type Item = StructItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next() {
            Ok(item) => Some(item),
            Err(_) => None,
        }
    }
}

#[derive(Debug)]
pub struct DtbReader<'a> {
    reserved_mem: &'a [ReservedMemEntry],
    struct_block: &'a [u8],
    strings_block: &'a [u8],
}

impl<'a> DtbReader<'a> {
    fn get_header(blob: &'a [u8]) -> Result<DtbHeader> {
        if blob.len() < 4 {
            return Err(Error::BadMagic);
        }

        let be_header =
            unsafe { &*(blob.as_ptr() as *const DtbHeader) as &DtbHeader };

        if u32::from_be(be_header.magic) != DTB_MAGIC {
            return Err(Error::BadMagic);
        }

        if blob.len() < size_of::<DtbHeader>() {
            return Err(Error::UnexpectedEndOfBlob);
        }

        let header = DtbHeader {
            magic: DTB_MAGIC,
            total_size: u32::from_be(be_header.total_size),
            struct_offset: u32::from_be(be_header.struct_offset),
            strings_offset: u32::from_be(be_header.strings_offset),
            reserved_mem_offset: u32::from_be(be_header.reserved_mem_offset),
            version: u32::from_be(be_header.version),
            last_comp_version: u32::from_be(be_header.last_comp_version),
            bsp_cpu_id: u32::from_be(be_header.bsp_cpu_id),
            strings_size: u32::from_be(be_header.strings_size),
            struct_size: u32::from_be(be_header.struct_size),
        };

        if header.version < header.last_comp_version {
            return Err(Error::BadVersion);
        }

        if header.last_comp_version != DTB_COMP_VERSION {
            return Err(Error::UnsupportedCompVersion);
        }

        if header.total_size as usize != blob.len() {
            return Err(Error::BadTotalSize);
        }

        Ok(header)
    }

    fn get_reserved_mem(
        blob: &'a [u8],
        header: &DtbHeader,
    ) -> Result<&'a [ReservedMemEntry]> {
        let entry_size = size_of::<ReservedMemEntry>();
        if header.reserved_mem_offset + entry_size as u32 > header.struct_offset
        {
            return Err(Error::OverlappingReservedMem);
        }

        if header.reserved_mem_offset % 8 != 0 {
            return Err(Error::UnalignedReservedMem);
        }

        let reserved_max_size =
            (header.struct_offset - header.reserved_mem_offset) as usize;
        let reserved = unsafe {
            let ptr = blob.as_ptr().offset(header.reserved_mem_offset as isize)
                as *const ReservedMemEntry;
            from_raw_parts(ptr, reserved_max_size / entry_size)
        };

        let index = reserved
            .iter()
            .position(|ref e| e.address == 0 && e.size == 0);
        if index.is_none() {
            return Err(Error::NoZeroReservedMemEntry);
        }

        Ok(&reserved[..index.unwrap()])
    }

    fn get_struct_block(
        blob: &'a [u8],
        header: &DtbHeader,
    ) -> Result<&'a [u8]> {
        if header.struct_offset % 4 != 0 || header.struct_size % 4 != 0 {
            return Err(Error::UnalignedStruct);
        }

        if header.struct_offset + header.struct_size > header.strings_offset {
            return Err(Error::OverlappingStruct);
        }

        let offset = header.struct_offset as usize;
        Ok(&blob[offset..offset + header.struct_size as usize])
    }

    fn get_strings_block(
        blob: &'a [u8],
        header: &DtbHeader,
    ) -> Result<&'a [u8]> {
        if header.strings_offset + header.strings_size > header.total_size {
            return Err(Error::OverlappingStrings);
        }

        let offset = header.strings_offset as usize;
        Ok(&blob[offset..offset + header.strings_size as usize])
    }

    pub fn new(blob: &'a [u8]) -> Result<Self> {
        let header = DtbReader::get_header(blob)?;
        Ok(DtbReader::<'a> {
            reserved_mem: DtbReader::get_reserved_mem(blob, &header)?,
            struct_block: DtbReader::get_struct_block(blob, &header)?,
            strings_block: DtbReader::get_strings_block(blob, &header)?,
        })
    }

    pub fn struct_iter(&self) -> DtbStructIterator<'a> {
        DtbStructIterator::<'a> {
            struct_block: self.struct_block,
            strings_block: self.strings_block,
            offset: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Read;
    use std::path::Path;

    fn new_reader<'a>(
        buf: &'a mut Vec<u8>,
        name: &str,
    ) -> Result<DtbReader<'a>> {
        let path = Path::new(file!()).parent().unwrap().join("test_dtb");
        let filename = path.join(String::from(name) + ".dtb");
        let mut file = File::open(filename).unwrap();
        buf.resize(0, 0);
        file.read_to_end(buf).unwrap();
        DtbReader::new(buf.as_slice())
    }

    macro_rules! test_new_reader {
        ($fn_name:ident, $err:ident) => {
            #[test]
            fn $fn_name() {
                let mut buf = Vec::new();
                let reader = new_reader(&mut buf, &stringify!($fn_name)[5..]);
                assert_eq!(reader.unwrap_err(), Error::$err);
            }
        };
    }

    test_new_reader!(test_bad_magic, BadMagic);
    test_new_reader!(test_unexpected_end_of_blob, UnexpectedEndOfBlob);
    test_new_reader!(test_bad_version, BadVersion);
    test_new_reader!(test_unsupported_comp_version, UnsupportedCompVersion);
    test_new_reader!(test_bad_total_size, BadTotalSize);
    test_new_reader!(test_unaligned_reserved_mem, UnalignedReservedMem);
    test_new_reader!(test_overlapping_reserved_mem, OverlappingReservedMem);
    test_new_reader!(test_no_zero_reserved_mem_entry, NoZeroReservedMemEntry);
    test_new_reader!(test_unaligned_struct, UnalignedStruct);
    test_new_reader!(test_unaligned_struct2, UnalignedStruct);
    test_new_reader!(test_overlapping_struct, OverlappingStruct);
    test_new_reader!(test_overlapping_strings, OverlappingStrings);

    fn assert_node<'a>(iter: &mut DtbStructIterator<'a>, name: &str) {
        let item = iter.next().unwrap();
        assert!(item.is_begin_node());
        assert_eq!(item.name().unwrap(), name);
    }

    fn assert_str_property<'a>(
        iter: &mut DtbStructIterator<'a>,
        name: &str,
        value: &str,
    ) {
        let item = iter.next().unwrap();
        assert!(item.is_property());
        assert_eq!(item.name().unwrap(), name);
        assert_eq!(item.value_str().unwrap(), value);
    }

    fn assert_str_list_property<'a>(
        iter: &mut DtbStructIterator<'a>,
        name: &str,
        value: &[&str],
    ) {
        let item = iter.next().unwrap();
        assert!(item.is_property());
        assert_eq!(item.name().unwrap(), name);
        let mut buf = [0; size_of::<&str>() * 8];
        assert_eq!(item.value_str_list(&mut buf).unwrap(), value);
    }

    fn assert_u32_list_property<'a>(
        iter: &mut DtbStructIterator<'a>,
        name: &str,
        value: &[u32],
    ) {
        let item = iter.next().unwrap();
        assert!(item.is_property());
        assert_eq!(item.name().unwrap(), name);
        let mut buf = [0; 4 * 8];
        assert_eq!(item.value_u32_list(&mut buf).unwrap(), value);
    }

    macro_rules! test_struct_iter {
        ($fn_name:ident, $err:ident) => {
            #[test]
            fn $fn_name() {
                let mut buf = Vec::new();
                let reader = new_reader(&mut buf, &stringify!($fn_name)[5..]);
                let mut iter = reader.unwrap().struct_iter();
                let err = loop {
                    match iter.next() {
                        Ok(_) => continue,
                        Err(err) => break err,
                    }
                };
                assert_eq!(err, Error::$err);
            }
        };
    }

    test_struct_iter!(test_unexpected_end_of_struct, UnexpectedEndOfStruct);
    test_struct_iter!(test_bad_struct_token, BadStructToken);
    test_struct_iter!(test_bad_node_name, BadNodeName);
    test_struct_iter!(test_unexpected_end_of_struct2, UnexpectedEndOfStruct);
    test_struct_iter!(test_unexpected_end_of_struct3, UnexpectedEndOfStruct);
    test_struct_iter!(test_bad_property_name, BadPropertyName);

    macro_rules! test_bad_str_encoding {
        ($fn_name:ident) => {
            #[test]
            fn $fn_name() {
                let mut buf = Vec::new();
                let reader = new_reader(&mut buf, &stringify!($fn_name)[5..]);
                let mut iter = reader.unwrap().struct_iter();
                loop {
                    match iter.next() {
                        Ok(_) => continue,
                        Err(Error::BadStrEncoding(_)) => break,
                        Err(err) => {
                            assert!(false, "unexpected error: {:?}", err)
                        }
                    }
                }
            }
        };
    }

    test_bad_str_encoding!(test_bad_str_encoding);
    test_bad_str_encoding!(test_bad_str_encoding2);

    #[test]
    fn test_struct_enum() {
        let mut buf = Vec::new();
        let mut iter = new_reader(&mut buf, "sample").unwrap().struct_iter();
        assert_node(&mut iter, "");
        assert_node(&mut iter, "node1");
        assert_str_property(&mut iter, "a-string-property", "A string");
        assert_str_list_property(
            &mut iter,
            "a-string-list-property",
            &["first string", "second string"],
        );
        assert_eq!(
            iter.next().unwrap(),
            StructItem::Property {
                name: "a-byte-data-property",
                value: &[0x01, 0x23, 0x34, 0x56],
            }
        );
        assert_node(&mut iter, "child-node1");
        assert_eq!(
            iter.next().unwrap(),
            StructItem::Property {
                name: "first-child-property",
                value: &[],
            }
        );
        assert_u32_list_property(&mut iter, "second-child-property", &[1]);
        assert_str_property(&mut iter, "a-string-property", "Hello, world");
        assert_eq!(iter.next().unwrap(), StructItem::EndNode);
        assert_node(&mut iter, "child-node2");
        assert_eq!(iter.next().unwrap(), StructItem::EndNode);
        assert_eq!(iter.next().unwrap(), StructItem::EndNode);
        assert_node(&mut iter, "node2");
        assert_eq!(
            iter.next().unwrap(),
            StructItem::Property {
                name: "an-empty-property",
                value: &[],
            }
        );
        assert_u32_list_property(&mut iter, "a-cell-property", &[1, 2, 3, 4]);
        assert_node(&mut iter, "child-node1");
        assert_eq!(iter.next().unwrap(), StructItem::EndNode);
        assert_eq!(iter.next().unwrap(), StructItem::EndNode);
        assert_eq!(iter.next().unwrap(), StructItem::EndNode);
        assert_eq!(iter.next().unwrap_err(), Error::NoMoreStructItems);
        assert_eq!(iter.next().unwrap_err(), Error::NoMoreStructItems);
    }

    fn assert_not_found<'a>(iter: &DtbStructIterator<'a>, path: &str) {
        let err = iter.find(path).unwrap_err();
        assert_eq!(err, Error::NoMoreStructItems);
    }

    fn name_from_path<'a>(path: &'a str) -> &'a str {
        path.trim_end_matches("/").rsplit("/").next().unwrap()
    }

    fn assert_begin_node_found<'a>(
        iter: &DtbStructIterator<'a>,
        path: &str,
    ) -> DtbStructIterator<'a> {
        let (item, iter) = iter.find(path).unwrap();
        assert!(item.is_begin_node());
        assert_eq!(item.name().unwrap(), name_from_path(path));
        iter
    }

    fn assert_property_found<'a>(
        iter: &DtbStructIterator<'a>,
        path: &str,
        value: &[u8],
    ) {
        let (item, _) = iter.find(path).unwrap();
        assert!(item.is_property());
        assert_eq!(item.name().unwrap(), name_from_path(path));
        assert_eq!(item.value().unwrap(), value);
    }

    #[test]
    fn test_find() {
        let mut buf = Vec::new();
        let root = new_reader(&mut buf, "sample").unwrap().struct_iter();

        assert_begin_node_found(&root, "");
        assert_begin_node_found(&root, "/");
        assert_not_found(&root, "//");

        let iter = assert_begin_node_found(&root, "/node1");
        assert_not_found(&root, "node1");

        let val = "A string\0".as_bytes();
        assert_not_found(&root, "/a-string-property");
        assert_not_found(&root, "a-string-property");
        assert_property_found(&iter, "a-string-property", val);
        assert_property_found(&root, "/node1/a-string-property", val);

        let iter = assert_begin_node_found(&root, "/node2");
        assert_begin_node_found(&root, "/node2/");
        assert_property_found(&root, "/node2/an-empty-property", &[]);
        assert_property_found(&iter, "an-empty-property", &[]);
        assert_not_found(&root, "an-empty-property");
        assert_not_found(&iter, "/node2/an-empty-property");

        assert_not_found(&iter, "node1/child-node1/a-string-property");
        assert_property_found(
            &root,
            "/node1/child-node1/a-string-property/",
            "Hello, world\0".as_bytes(),
        );
    }
}
