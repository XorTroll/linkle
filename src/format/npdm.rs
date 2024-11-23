use crate::format::utils;
use crate::format::utils::HexOrNum;
use crate::format::svc;
use crate::error::Error;
use bit_field::BitField;
use bincode::Options;
use serde_derive::{Deserialize, Serialize};
use snafu::GenerateBacktrace;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::path::Path;
use std::mem::size_of;
use std::io::Write;
use snafu::Backtrace;
use rsa::{BigUint, RSAPrivateKey};

mod fmt;
use fmt::*;

// TODO: Pretty errors if the user messes up.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum SystemCalls {
    /// Accepts the standard svcName: 0x<svc_id_hex>
    KeyValue(HashMap<String, HexOrNum>),
    /// Accepts syscall names. Those must be correctly spelled.
    Name(Vec<svc::SystemCallId>),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum ProgramType {
    Value(HexOrNum),
    Name(String)
}

impl ProgramType {
    pub fn get_value(&self) -> Option<u16> {
        match self {
            ProgramType::Value(prog_type_val) => {
                if prog_type_val.0 > 2 {
                    None
                }
                else {
                    Some(prog_type_val.0 as u16)
                }
            }
            ProgramType::Name(prog_type_str) => {
                match prog_type_str.to_lowercase().as_str() {
                    "system" => Some(0),
                    "application" => Some(1),
                    "applet" => Some(2),
                    _ => None
                }
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum KernelVersion {
    Value(HexOrNum),
    Version(String)
}

impl KernelVersion {
    pub fn get_value(&self) -> Option<u16> {
        match self {
            KernelVersion::Value(ver_val) => {
                if ver_val.0 < 0x030 {
                    None
                }
                else {
                    Some(ver_val.0 as u16)
                }
            },
            KernelVersion::Version(ver_str) => {
                let ver_strs: Vec<&str> = ver_str.split('.').collect();
                if ver_strs.len() == 2 {
                    if let Ok(major) = u32::from_str_radix(ver_strs[0], 10) {
                        if let Ok(minor) = u32::from_str_radix(ver_strs[1], 10) {
                            return Some(*0u16.set_bits(0..4, minor as u16).set_bits(4..16, major as u16))
                        }
                    }
                }

                return None;
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", content = "value")]
#[serde(rename_all = "snake_case")]
pub enum KernelCapability {
    #[serde(alias = "kernel_flags")]
    ThreadInfo {
        #[serde(alias = "highest_thread_priority")]
        highest_priority: u8,
        #[serde(alias = "lowest_thread_priority")]
        lowest_priority: u8,
        #[serde(alias = "highest_cpu_id")]
        max_core_number: u8,
        #[serde(alias = "lowest_cpu_id")]
        min_core_number: u8,
    },
    #[serde(alias = "syscalls")]
    EnableSystemCalls(SystemCalls),
    #[serde(alias = "map")]
    MemoryMap {
        address: HexOrNum,
        size: HexOrNum,
        is_ro: bool,
        is_io: bool,
    },
    #[serde(alias = "map_page")]
    IoMemoryMap(HexOrNum),
    #[serde(alias = "irq_pair")]
    EnableInterrupts([u16; 2]),
    #[serde(alias = "application_type")]
    MiscParams(ProgramType),
    #[serde(alias = "min_kernel_version")]
    KernelVersion(KernelVersion),
    HandleTableSize(u16),
    #[serde(alias = "debug_flags")]
    DebugFlags {
        allow_debug: bool,
        force_debug_prod: bool,
        force_debug: bool
    },
}

fn encode_syscalls<I: Iterator<Item=u32>>(syscalls: I) -> Vec<u32> {
    let mut masks = vec![0b1111u32; 6];
    let mut used = [false; 6];
    for (idx, mask) in masks.iter_mut().enumerate() {
        mask.set_bits(29..32, idx as u32);
    }
    for syscall_val in syscalls {
        masks[syscall_val as usize / 24].set_bit(usize::try_from((syscall_val % 24) + 5).unwrap(), true);
        used[syscall_val as usize / 24] = true;
    }
    for (idx, used) in used.iter().enumerate().rev() {
        if !used {
            masks.remove(idx);
        }
    }
    masks
}

impl KernelCapability {
    pub fn encode(&self) -> Result<Vec<u32>, Error> {
        match self {
            KernelCapability::ThreadInfo {
                highest_priority,
                lowest_priority,
                max_core_number,
                min_core_number,
            } => {
                Ok(vec![*0b111u32
                    .set_bits(04..10, u32::from(*highest_priority))
                    .set_bits(10..16, u32::from(*lowest_priority))
                    .set_bits(16..24, u32::from(*min_core_number))
                    .set_bits(24..32, u32::from(*max_core_number))])
            },
            KernelCapability::EnableSystemCalls(SystemCalls::Name(syscalls)) => {
                Ok(encode_syscalls(syscalls.iter().map(|v| *v as u32)))
            },
            KernelCapability::EnableSystemCalls(SystemCalls::KeyValue(syscalls)) => {
                Ok(encode_syscalls(syscalls.iter().map(|(_, v)| v.0 as u32)))
            },
            KernelCapability::MemoryMap {
                address,
                size,
                is_ro,
                is_io,
            } => {
                let mut val = vec![0b11_1111u32, 0b11_1111u32];
                val[0]
                    .set_bits(7..31, u32::try_from(address.0).unwrap())
                    .set_bit(31, *is_ro);
                val[1]
                    .set_bits(7..31, u32::try_from(size.0).unwrap())
                    .set_bit(31, *is_io);
                Ok(val)
            }
            KernelCapability::IoMemoryMap(page) => {
                Ok(vec![*0b111_1111u32.set_bits(8..32, u32::try_from(page.0).unwrap())])
            }
            KernelCapability::EnableInterrupts(irq_pair) => Ok(vec![*0b111_1111_1111u32
                .set_bits(12..22, u32::from(irq_pair[0]))
                .set_bits(22..32, u32::from(irq_pair[1]))]),
            KernelCapability::MiscParams(prog_type) => {
                match prog_type.get_value() {
                    None => Err(Error::InvalidNpdmValue { error: "misc_params (program_type)".into(), backtrace: Backtrace::generate() }),
                    Some(prog_type_val) => Ok(vec![*0b1_1111_1111_1111u32.set_bits(14..17, prog_type_val as u32)])
                }
            }
            KernelCapability::KernelVersion(kern_ver) => {
                match kern_ver.get_value() {
                    None => Err(Error::InvalidNpdmValue { error: "kernel_version".into(), backtrace: Backtrace::generate() }),
                    Some(kern_ver_val) => Ok(vec![*0b11_1111_1111_1111u32.set_bits(15..32, kern_ver_val as u32)])
                }
            }
            KernelCapability::HandleTableSize(handle_table_size) => {
                Ok(vec![*0b111_1111_1111_1111u32.set_bits(16..26, u32::from(*handle_table_size))])
            }
            KernelCapability::DebugFlags {
                allow_debug,
                force_debug,
                force_debug_prod
            } => {
                if ((*allow_debug as u32) + (*force_debug as u32) + (*force_debug_prod as u32)) > 1 {
                    Err(Error::InvalidNpdmDebugFlags { backtrace: Backtrace::generate() })
                }
                else {
                    Ok(vec![*0b1111_1111_1111_1111u32
                        .set_bit(17, *allow_debug)
                        .set_bit(18, *force_debug_prod)
                        .set_bit(19, *force_debug)])
                }
            }
        }
    }
}

fn sac_encoded_len(sacs: &[String]) -> usize {
    sacs.iter().map(|v| 1 + v.len()).sum()
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EnabledSystemCall {
    Value(HexOrNum),
    Name(svc::SystemCallId)
}

impl EnabledSystemCall {
    #[inline]
    pub fn get_id(&self) -> svc::SystemCallId {
        match self {
            EnabledSystemCall::Value(raw_val) => unsafe {
                std::mem::transmute(raw_val.0 as u32)
            },
            EnabledSystemCall::Name(id) => *id
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MemoryMap {
    address: HexOrNum,
    size: HexOrNum,
    is_ro: bool,
    is_io: bool   
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KernelCapabilityValues {
    highest_priority: u8,
    lowest_priority: u8,
    max_core_number: u8,
    min_core_number: u8,
    enable_system_calls: Vec<EnabledSystemCall>,
    memory_maps: Option<Vec<MemoryMap>>,
    io_memory_maps: Option<Vec<HexOrNum>>,
    enable_interrupts: Option<Vec<[u16; 2]>>,
    program_type: Option<ProgramType>,
    kernel_version: Option<KernelVersion>,
    allow_debug: Option<bool>,
    force_debug_prod: Option<bool>,
    force_debug: Option<bool>
}

impl KernelCapabilityValues {
    pub fn to_list_format(&self) -> Vec<KernelCapability> {
        let mut kern_caps: Vec<KernelCapability> = Vec::new();

        let thread_info_kcap = KernelCapability::ThreadInfo {
            highest_priority: self.highest_priority,
            lowest_priority: self.lowest_priority,
            max_core_number: self.max_core_number,
            min_core_number: self.min_core_number
        };
        kern_caps.push(thread_info_kcap);

        let enable_svc_ids: Vec<svc::SystemCallId> = self.enable_system_calls.iter().map(|svc| svc.get_id()).collect();
        let enable_svcs_kcap = KernelCapability::EnableSystemCalls(SystemCalls::Name(enable_svc_ids));
        kern_caps.push(enable_svcs_kcap);

        if let Some(mem_maps) = &self.memory_maps {
            for mem_map in mem_maps.iter() {
                let mem_map_kcap = KernelCapability::MemoryMap {
                    address: mem_map.address,
                    size: mem_map.size,
                    is_ro: mem_map.is_ro,
                    is_io: mem_map.is_io
                };
                kern_caps.push(mem_map_kcap);
            }
        }

        if let Some(io_mem_maps) = &self.io_memory_maps {
            for io_mem_map in io_mem_maps.iter() {
                let io_mem_map_kcap = KernelCapability::IoMemoryMap(*io_mem_map);
                kern_caps.push(io_mem_map_kcap);
            }
        }

        if let Some(enable_ints) = &self.enable_interrupts {
            for enable_int in enable_ints.iter() {
                let enable_int_kcap = KernelCapability::EnableInterrupts(*enable_int);
                kern_caps.push(enable_int_kcap);
            }
        }

        if let Some(program_type) = &self.program_type {
            let misc_params_kcap = KernelCapability::MiscParams(program_type.clone());
            kern_caps.push(misc_params_kcap);
        }

        if let Some(kernel_version) = &self.kernel_version {
            let kern_version_kcap = KernelCapability::KernelVersion(kernel_version.clone());
            kern_caps.push(kern_version_kcap);
        }

        if self.allow_debug.is_some() || self.force_debug_prod.is_some() || self.force_debug.is_some() {
            let debug_flags_kcap = KernelCapability::DebugFlags {
                allow_debug: self.allow_debug.unwrap_or(false),
                force_debug_prod: self.force_debug_prod.unwrap_or(false),
                force_debug: self.force_debug.unwrap_or(false)
            };
            kern_caps.push(debug_flags_kcap);
        }

        kern_caps
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FsAccessControl {
    #[serde(alias = "permissions")]
    flags: HexOrNum
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceAccessControl {
    accessed_services: Vec<String>,
    hosted_services: Vec<String>
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KernelCapabilities {
    TypeValueList(Vec<KernelCapability>),
    Struct(KernelCapabilityValues)
}

impl KernelCapabilities {
    pub fn get_list(&self) -> Vec<KernelCapability> {
        match self {
            KernelCapabilities::TypeValueList(list) => list.to_vec(),
            KernelCapabilities::Struct(kern_caps) => kern_caps.to_list_format()
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Npdm {
    // META fields
    name: String,
    product_code: Option<String>,
    signature_key_generation: Option<u32>,
    main_thread_stack_size: HexOrNum,
    main_thread_priority: u8,
    #[serde(alias = "default_cpu_id")]
    main_thread_core_number: u8,
    system_resource_size: Option<u32>,
    #[serde(alias = "process_category")] // We thought this field was the process_category. We were wrong. 🤦
    version: Option<u32>,
    address_space_type: u8,
    is_64_bit: bool,
    optimize_memory_allocation: Option<bool>,
    disable_device_address_space_merge: Option<bool>,

    // ACID fields
    #[serde(alias = "is_retail")]
    is_production: Option<bool>,
    unqualified_approval: Option<bool>,
    #[serde(alias = "pool_partition")]
    memory_region: u32,
    #[serde(alias = "title_id_range_min")]
    program_id_range_min: Option<HexOrNum>,
    #[serde(alias = "title_id_range_max")]
    program_id_range_max: Option<HexOrNum>,

    // ACI0 fields
    #[serde(alias = "title_id")]
    program_id: HexOrNum,

    // FAC
    #[serde(alias = "filesystem_access")]
    fs_access_control: FsAccessControl,

    // SAC
    #[serde(alias = "service_access")]
    accessed_services: Option<Vec<String>>,
    #[serde(alias = "service_host")]
    hosted_services: Option<Vec<String>>,
    service_access_control: Option<ServiceAccessControl>,

    // KAC
    kernel_capabilities: KernelCapabilities,

    // Other
    developer_key: Option<String>
}

pub enum AcidBehavior<'a> {
    Sign { pem_file_path: &'a Path },
    Empty,
    Use { acid_file_path: &'a Path }
}

impl Npdm {
    pub fn from_json(file: &Path) -> Result<Npdm, Error> {
        let file = std::fs::File::open(file)?;
        match serde_json::from_reader(file) {
            Ok(res) => Ok(res),
            Err(error) => Err(Error::from(error)),
        }
    }

    // TODO: Optionally pass a (signed) ACID here.
    pub fn into_npdm<W: Write>(&self, mut file: W, acid_behavior: AcidBehavior) -> Result<(), Error> {
        let mut meta: Meta = Meta::default();

        meta.magic = *b"META";
        meta.signature_key_generation = self.signature_key_generation.unwrap_or(0);

        if self.address_space_type & !3 != 0 {
            return Err(Error::InvalidNpdmValue {
                error: "address_space_type".into(),
                backtrace: Backtrace::generate()
            });
        }

        meta.flags = (self.address_space_type & 3) << 1;
        if self.is_64_bit {
            meta.flags |= 1 << 0;
        }
        if self.optimize_memory_allocation.unwrap_or(false) {
            meta.flags |= 1 << 4;
        }
        if self.disable_device_address_space_merge.unwrap_or(false) {
            meta.flags |= 1 << 5;
        }

        meta.main_thread_priority = self.main_thread_priority;
        meta.main_thread_core_number = self.main_thread_core_number;

        meta.system_resource_size = self.system_resource_size.unwrap_or(0);
        meta.version = self.version.unwrap_or(0);

        meta.main_thread_stack_size = self.main_thread_stack_size.0 as _;

        let name_len = std::cmp::min(self.name.as_bytes().len(), 12);
        meta.name = [0; 0x10];
        meta.name[..name_len].copy_from_slice(&self.name.as_bytes()[..name_len]);

        meta.product_code = [0; 0x10];

        let accessed_services = if let Some(sac) = self.service_access_control.as_ref() {
            &sac.accessed_services
        }
        else if let Some(accessed_srvs) = self.accessed_services.as_ref() {
            accessed_srvs
        }
        else {
            panic!("No accessed srvs!");
        };
        let hosted_services = if let Some(sac) = self.service_access_control.as_ref() {
            &sac.hosted_services
        }
        else if let Some(hosted_srvs) = self.hosted_services.as_ref() {
            hosted_srvs
        }
        else {
            panic!("No hosted srvs!");
        };

        let kern_caps = self.kernel_capabilities.get_list();

        meta.acid_offset = size_of::<Meta>() as u32;
        meta.acid_size = match acid_behavior {
            AcidBehavior::Sign { .. } | AcidBehavior::Empty => {
                (0x100 + size_of::<Acid>() + size_of::<AcidFsAccessControl>() +
                    sac_encoded_len(&hosted_services) + sac_encoded_len(&accessed_services) +
                    kern_caps.iter().map(|v| v.encode().unwrap().len() * 4).sum::<usize>()) as u32
            },
            AcidBehavior::Use { acid_file_path } => std::fs::metadata(acid_file_path)?.len() as u32,
        };

        meta.aci_offset = meta.acid_offset + meta.acid_size;
        meta.aci_size = (size_of::<Aci>() + size_of::<AciFsAccessControl>() +
            sac_encoded_len(&hosted_services) + sac_encoded_len(&accessed_services) +
            kern_caps.iter().map(|v| v.encode().unwrap().len() * 4).sum::<usize>()) as u32;

            bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes().with_no_limit().with_little_endian().serialize_into(&mut file, &meta)?;

        match acid_behavior {
            AcidBehavior::Sign { pem_file_path } => {
                // Parse PEM file
                let pkey = get_pkey_from_pem(pem_file_path)?;

                let mut v = Vec::new();
                write_acid(&mut v, self, &meta, accessed_services, hosted_services, &kern_caps)?;
                println!("Signing over {:02x?}", v);

                // calculate signature.
                let hash = utils::calculate_sha256(v.as_slice())?;
                println!("Signing over {:02x?}", hash);
                let sig = pkey.sign(rsa::PaddingScheme::new_pss::<sha2::Sha256, _>(rand::thread_rng()), &hash)?;
                assert_eq!(sig.len(), 0x100, "Signature of wrong length generated");
                file.write_all(&sig)?;

                write_acid(&mut file, self, &meta, accessed_services, hosted_services, &kern_caps)?;
            },
            AcidBehavior::Empty => {
                file.write_all(&[0; 0x100])?;
                write_acid(&mut file, self, &meta, accessed_services, hosted_services, &kern_caps)?;
            }
            AcidBehavior::Use { acid_file_path } => {
                let mut acid_file = std::fs::File::open(acid_file_path)?;
                std::io::copy(&mut acid_file, &mut file)?;
            }
        }

        // ACI0
        let mut aci0 = Aci::default();
        aci0.magic = *b"ACI0";
        aci0.program_id = self.program_id.0;
        aci0.fs_access_control_offset = size_of::<Aci>() as u32;
        aci0.fs_access_control_size = size_of::<AciFsAccessControl>() as u32;
        aci0.service_access_control_offset = aci0.fs_access_control_offset + aci0.fs_access_control_size;
        aci0.service_access_control_size = (sac_encoded_len(&hosted_services) + sac_encoded_len(&accessed_services)) as u32;
        aci0.kernel_access_control_offset = aci0.service_access_control_offset + aci0.service_access_control_size;
        aci0.kernel_access_control_size = kern_caps.iter().map(|v| v.encode().unwrap().len() * 4).sum::<usize>() as u32;

        bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes().with_no_limit().with_little_endian().serialize_into(&mut file, &aci0)?;

        let mut aci0_fac = AciFsAccessControl::default();
        aci0_fac.version = 1;
        aci0_fac.padding = [0; 3];
        aci0_fac.fs_access_flags_bitmask.copy_from_slice(&self.fs_access_control.flags.0.to_le_bytes());
        aci0_fac.content_owner_info_offset = 0x1C; // Always 0x1C
        aci0_fac.content_owner_info_size = 0;
        aci0_fac.save_data_owner_info_offset = 0x1C;
        aci0_fac.save_data_owner_info_size = 0;

        bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes().with_no_limit().with_little_endian().serialize_into(&mut file, &aci0_fac)?;

        for elem in accessed_services {
            if elem.len() & !7 != 0 || elem.len() == 0 {
                return Err(Error::InvalidNpdmValue {
                    error: format!("accessed_services.{}", elem).into(),
                    backtrace: Backtrace::generate()
                });
            }
            file.write_all(&[elem.len() as u8 - 1])?;
            file.write_all(elem.as_bytes())?;
        }

        for elem in hosted_services {
            if elem.len() & !7 != 0 || elem.len() == 0 {
                return Err(Error::InvalidNpdmValue {
                    error: format!("hosted_services.{}", elem).into(),
                    backtrace: Backtrace::generate()
                });
            }
            file.write_all(&[0x80 | (elem.len() as u8 - 1)])?;
            file.write_all(elem.as_bytes())?;
        }

        for elem in &kern_caps {
            let encoded = elem.encode()?.iter().map(|v| v.to_le_bytes().to_vec()).flatten().collect::<Vec<u8>>();
            file.write_all(&encoded)?;
        }

        Ok(())
    }
}

fn get_pkey_from_pem(path: &Path) -> Result<RSAPrivateKey, Error> {
    let data = std::fs::read_to_string(path)?;
    let data = pem::parse(data)?.contents;

    let (n, e, d, prime1, prime2) = yasna::parse_der(&data, |reader| {
        reader.read_sequence(|reader| {
            let _v = reader.next().read_i64()?;
            let _oid = reader.next().read_sequence(|reader| {
                reader.next().read_oid()
            })?;
            let bytes = reader.next().read_bytes()?;
            yasna::parse_der(&bytes, |reader| reader.read_sequence(|reader| {
                let _v = reader.next().read_i64()?;
                let modulus = reader.next().read_biguint()?;
                let pubexp = reader.next().read_biguint()?;
                let privexp = reader.next().read_biguint()?;
                let prime1 = reader.next().read_biguint()?;
                let prime2 = reader.next().read_biguint()?;
                let _exp1 = reader.next().read_biguint()?;
                let _exp2 = reader.next().read_biguint()?;
                let _coeff = reader.next().read_biguint()?;
                Ok((modulus, pubexp, privexp, prime1, prime2))
            }))
        })
    })?;

    let pkey = rsa::RSAPrivateKey::from_components(
        BigUint::from_bytes_be(&n.to_bytes_be()),
        BigUint::from_bytes_be(&e.to_bytes_be()),
        BigUint::from_bytes_be(&d.to_bytes_be()),
        vec![
            BigUint::from_bytes_be(&prime1.to_bytes_be()),
            BigUint::from_bytes_be(&prime2.to_bytes_be()),
        ]
    );
    pkey.validate()?;

    Ok(pkey)
}

fn write_acid<T: Write>(mut writer: &mut T, npdm: &Npdm, meta: &Meta, accessed_services: &Vec<String>, hosted_services: &Vec<String>, kern_caps: &Vec<KernelCapability>) -> Result<(), Error> {
    let mut acid = Acid::default();

    if let Some(devkey) = &npdm.developer_key {
        acid.rsa_nca_pubkey.0.copy_from_slice(&hex::decode(devkey).unwrap());
    }

    acid.magic = *b"ACID";
    acid.signed_size = meta.acid_size - 0x100;

    acid.flags = 0u32;
    if npdm.is_production.unwrap_or(true) {
        acid.flags |= 1 << 0;
    }
    if npdm.unqualified_approval.unwrap_or(false) {
        acid.flags |= 1 << 1;
    }

    if npdm.memory_region & !3 != 0 {
        return Err(Error::InvalidNpdmValue {
            error: format!("memory_region").into(),
            backtrace: Backtrace::generate()
        });
    }
    acid.flags |= (npdm.memory_region & 3) << 2;

    acid.program_id_range_min = npdm.program_id_range_min.as_ref().unwrap_or(&npdm.program_id).0;
    acid.program_id_range_max = npdm.program_id_range_max.as_ref().unwrap_or(&npdm.program_id).0;

    acid.fs_access_control_offset = 0x100 + size_of::<Acid>() as u32;
    acid.fs_access_control_size = size_of::<AcidFsAccessControl>() as u32;

    acid.service_access_control_offset = acid.fs_access_control_offset + acid.fs_access_control_size;
    acid.service_access_control_size = (sac_encoded_len(hosted_services) + sac_encoded_len(accessed_services)) as u32;

    acid.kernel_access_control_offset = acid.service_access_control_offset + acid.service_access_control_size;
    acid.kernel_access_control_size = kern_caps.iter().map(|v| v.encode().unwrap().len() * 4).sum::<usize>() as u32;

    let mut acid_fac = AcidFsAccessControl::default();
    acid_fac.version = 1;
    acid_fac.content_owner_id_count = 0;
    acid_fac.save_data_owner_id_count = 0;
    acid_fac.padding = 0;
    acid_fac.fs_access_flags_bitmask.copy_from_slice(&npdm.fs_access_control.flags.0.to_le_bytes());
    acid_fac.content_owner_id_min = 0;
    acid_fac.content_owner_id_max = 0;
    acid_fac.save_data_owner_id_min = 0;
    acid_fac.save_data_owner_id_max = 0;

    let mut final_size = bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes().with_no_limit().with_little_endian().serialized_size(&acid)?;
    assert_eq!(final_size as usize, size_of::<Acid>(), "Serialized ACID has wrong size");
    bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes().with_no_limit().with_little_endian().serialize_into(&mut writer, &acid)?;

    final_size += bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes().with_no_limit().with_little_endian().serialized_size(&acid_fac)?;
    assert_eq!(final_size as usize, size_of::<Acid>() + size_of::<AcidFsAccessControl>(), "Serialized FAC has wrong size");
    bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes().with_no_limit().with_little_endian().serialize_into(&mut writer, &acid_fac)?;

    for elem in accessed_services {
        if elem.len() & !7 != 0 || elem.len() == 0 {
            return Err(Error::InvalidNpdmValue {
                error: format!("accessed_services.{}", elem).into(),
                backtrace: Backtrace::generate()
            });
        }
        final_size += 1;
        writer.write_all(&[elem.len() as u8 - 1])?;
        final_size += elem.as_bytes().len() as u64;
        writer.write_all(elem.as_bytes())?;
    }

    for elem in hosted_services {
        if elem.len() & !7 != 0 || elem.len() == 0 {
            return Err(Error::InvalidNpdmValue {
                error: format!("hosted_services.{}", elem).into(),
                backtrace: Backtrace::generate()
            });
        }
        final_size += 1;
        writer.write_all(&[0x80 | (elem.len() as u8 - 1)])?;
        final_size += elem.as_bytes().len() as u64;
        writer.write_all(elem.as_bytes())?;
    }

    assert_eq!(final_size as usize, size_of::<Acid>() + size_of::<AcidFsAccessControl>()
        + sac_encoded_len(accessed_services) + sac_encoded_len(hosted_services), "Serialized SAC has wrong size");

    for elem in kern_caps {
        let encoded = elem.encode()?.iter().map(|v| v.to_le_bytes().to_vec()).flatten().collect::<Vec<u8>>();
        final_size += encoded.len() as u64;
        writer.write_all(&encoded)?;
    }

    assert_eq!(final_size as usize, size_of::<Acid>() + size_of::<AcidFsAccessControl>()
        + sac_encoded_len(accessed_services) + sac_encoded_len(hosted_services)
        + kern_caps.iter().map(|v| v.encode().unwrap().len() * 4).sum::<usize>(), "Serialized KAC has wrong size");

    Ok(())
}