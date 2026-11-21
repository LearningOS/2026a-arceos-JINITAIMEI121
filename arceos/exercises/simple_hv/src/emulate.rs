use crate::regs::{GeneralPurposeRegisters, GprIndex};

const VIRTUAL_HART_ID: usize = 0x1234;
const DEVICE_ADDRESS: usize = 0x40;
const DEVICE_VALUE: usize = 0x6688;

pub fn csr_read(instruction: u32, regs: &mut GeneralPurposeRegisters) -> bool {
    let operation = (instruction >> 12) & 7;
    if instruction & 0x7f != 0x73
        || instruction >> 20 != 0xf14
        || (instruction >> 15) & 31 != 0
        || !matches!(operation, 2 | 3 | 6 | 7)
    {
        return false;
    }
    let destination = GprIndex::from_raw((instruction >> 7) & 31).unwrap();
    regs.set_reg(destination, VIRTUAL_HART_ID);
    true
}

pub fn mmio_read(
    instruction: u32,
    fault_address: usize,
    regs: &mut GeneralPurposeRegisters,
) -> bool {
    if instruction & 0x707f != 0x3003 || fault_address != DEVICE_ADDRESS {
        return false;
    }
    let source = GprIndex::from_raw((instruction >> 15) & 31).unwrap();
    let displacement = ((instruction as i32) >> 20) as isize;
    if regs.reg(source).wrapping_add(displacement as usize) != DEVICE_ADDRESS {
        return false;
    }
    let destination = GprIndex::from_raw((instruction >> 7) & 31).unwrap();
    regs.set_reg(destination, DEVICE_VALUE);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csr_destination_is_decoded_and_zero_stays_zero() {
        let mut regs = GeneralPurposeRegisters::default();
        assert!(csr_read(0xf1402573, &mut regs));
        assert_eq!(regs.reg(GprIndex::A0), VIRTUAL_HART_ID);
        assert!(csr_read(0xf1402073, &mut regs));
        assert_eq!(regs.reg(GprIndex::Zero), 0);
        assert!(!csr_read(0xf1409573, &mut regs));
        assert!(!csr_read(0x30002573, &mut regs));
    }

    #[test]
    fn device_load_uses_decoded_address_and_destination() {
        let mut regs = GeneralPurposeRegisters::default();
        assert!(mmio_read(0x04003503, DEVICE_ADDRESS, &mut regs));
        assert_eq!(regs.reg(GprIndex::A0), DEVICE_VALUE);
        assert!(!mmio_read(0x04003503, DEVICE_ADDRESS + 8, &mut regs));
        assert!(!mmio_read(0x04803503, DEVICE_ADDRESS, &mut regs));
        assert!(!mmio_read(0x04002503, DEVICE_ADDRESS, &mut regs));
        regs.set_reg(GprIndex::A1, 0x48);
        assert!(mmio_read(0xff85b603, DEVICE_ADDRESS, &mut regs));
        assert_eq!(regs.reg(GprIndex::A2), DEVICE_VALUE);
    }
}
