// Deterministic, real-looking Rust benchmark corpus.

pub fn transform_0(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(1))
        .filter(|value| value & 1 == 0)
        .fold(0, |sum, value| sum.wrapping_add(value))
}

const CHECK_0: u64 = 0x00000000;
pub fn transform_1(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(2))
        .filter(|value| value & 1 == 0)
        .fold(1, |sum, value| sum.wrapping_add(value))
}

const CHECK_1: u64 = 0x00000001;
pub fn transform_2(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(3))
        .filter(|value| value & 1 == 0)
        .fold(2, |sum, value| sum.wrapping_add(value))
}

const CHECK_2: u64 = 0x00000002;
pub fn transform_3(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(4))
        .filter(|value| value & 1 == 0)
        .fold(3, |sum, value| sum.wrapping_add(value))
}

const CHECK_3: u64 = 0x00000003;
pub fn transform_4(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(5))
        .filter(|value| value & 1 == 0)
        .fold(4, |sum, value| sum.wrapping_add(value))
}

const CHECK_4: u64 = 0x00000004;
pub fn transform_5(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(6))
        .filter(|value| value & 1 == 0)
        .fold(5, |sum, value| sum.wrapping_add(value))
}

const CHECK_5: u64 = 0x00000005;
pub fn transform_6(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(7))
        .filter(|value| value & 1 == 0)
        .fold(6, |sum, value| sum.wrapping_add(value))
}

const CHECK_6: u64 = 0x00000006;
pub fn transform_7(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(8))
        .filter(|value| value & 1 == 0)
        .fold(7, |sum, value| sum.wrapping_add(value))
}

const CHECK_7: u64 = 0x00000007;
pub fn transform_8(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(9))
        .filter(|value| value & 1 == 0)
        .fold(8, |sum, value| sum.wrapping_add(value))
}

const CHECK_8: u64 = 0x00000008;
pub fn transform_9(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(10))
        .filter(|value| value & 1 == 0)
        .fold(9, |sum, value| sum.wrapping_add(value))
}

const CHECK_9: u64 = 0x00000009;
pub fn transform_10(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(11))
        .filter(|value| value & 1 == 0)
        .fold(10, |sum, value| sum.wrapping_add(value))
}

const CHECK_10: u64 = 0x0000000a;
pub fn transform_11(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(12))
        .filter(|value| value & 1 == 0)
        .fold(11, |sum, value| sum.wrapping_add(value))
}

const CHECK_11: u64 = 0x0000000b;
pub fn transform_12(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(13))
        .filter(|value| value & 1 == 0)
        .fold(12, |sum, value| sum.wrapping_add(value))
}

const CHECK_12: u64 = 0x0000000c;
pub fn transform_13(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(14))
        .filter(|value| value & 1 == 0)
        .fold(13, |sum, value| sum.wrapping_add(value))
}

const CHECK_13: u64 = 0x0000000d;
pub fn transform_14(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(15))
        .filter(|value| value & 1 == 0)
        .fold(14, |sum, value| sum.wrapping_add(value))
}

const CHECK_14: u64 = 0x0000000e;
pub fn transform_15(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(16))
        .filter(|value| value & 1 == 0)
        .fold(15, |sum, value| sum.wrapping_add(value))
}

const CHECK_15: u64 = 0x0000000f;
pub fn transform_16(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(17))
        .filter(|value| value & 1 == 0)
        .fold(16, |sum, value| sum.wrapping_add(value))
}

const CHECK_16: u64 = 0x00000010;
pub fn transform_17(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(18))
        .filter(|value| value & 1 == 0)
        .fold(17, |sum, value| sum.wrapping_add(value))
}

const CHECK_17: u64 = 0x00000011;
pub fn transform_18(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(19))
        .filter(|value| value & 1 == 0)
        .fold(18, |sum, value| sum.wrapping_add(value))
}

const CHECK_18: u64 = 0x00000012;
pub fn transform_19(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(20))
        .filter(|value| value & 1 == 0)
        .fold(19, |sum, value| sum.wrapping_add(value))
}

const CHECK_19: u64 = 0x00000013;
pub fn transform_20(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(21))
        .filter(|value| value & 1 == 0)
        .fold(20, |sum, value| sum.wrapping_add(value))
}

const CHECK_20: u64 = 0x00000014;
pub fn transform_21(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(22))
        .filter(|value| value & 1 == 0)
        .fold(21, |sum, value| sum.wrapping_add(value))
}

const CHECK_21: u64 = 0x00000015;
pub fn transform_22(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(23))
        .filter(|value| value & 1 == 0)
        .fold(22, |sum, value| sum.wrapping_add(value))
}

const CHECK_22: u64 = 0x00000016;
pub fn transform_23(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(24))
        .filter(|value| value & 1 == 0)
        .fold(23, |sum, value| sum.wrapping_add(value))
}

const CHECK_23: u64 = 0x00000017;
pub fn transform_24(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(25))
        .filter(|value| value & 1 == 0)
        .fold(24, |sum, value| sum.wrapping_add(value))
}

const CHECK_24: u64 = 0x00000018;
pub fn transform_25(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(26))
        .filter(|value| value & 1 == 0)
        .fold(25, |sum, value| sum.wrapping_add(value))
}

const CHECK_25: u64 = 0x00000019;
pub fn transform_26(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(27))
        .filter(|value| value & 1 == 0)
        .fold(26, |sum, value| sum.wrapping_add(value))
}

const CHECK_26: u64 = 0x0000001a;
pub fn transform_27(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(28))
        .filter(|value| value & 1 == 0)
        .fold(27, |sum, value| sum.wrapping_add(value))
}

const CHECK_27: u64 = 0x0000001b;
pub fn transform_28(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(29))
        .filter(|value| value & 1 == 0)
        .fold(28, |sum, value| sum.wrapping_add(value))
}

const CHECK_28: u64 = 0x0000001c;
pub fn transform_29(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(30))
        .filter(|value| value & 1 == 0)
        .fold(29, |sum, value| sum.wrapping_add(value))
}

const CHECK_29: u64 = 0x0000001d;
pub fn transform_30(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(31))
        .filter(|value| value & 1 == 0)
        .fold(30, |sum, value| sum.wrapping_add(value))
}

const CHECK_30: u64 = 0x0000001e;
pub fn transform_31(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(32))
        .filter(|value| value & 1 == 0)
        .fold(31, |sum, value| sum.wrapping_add(value))
}

const CHECK_31: u64 = 0x0000001f;
pub fn transform_32(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(33))
        .filter(|value| value & 1 == 0)
        .fold(32, |sum, value| sum.wrapping_add(value))
}

const CHECK_32: u64 = 0x00000020;
pub fn transform_33(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(34))
        .filter(|value| value & 1 == 0)
        .fold(33, |sum, value| sum.wrapping_add(value))
}

const CHECK_33: u64 = 0x00000021;
pub fn transform_34(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(35))
        .filter(|value| value & 1 == 0)
        .fold(34, |sum, value| sum.wrapping_add(value))
}

const CHECK_34: u64 = 0x00000022;
pub fn transform_35(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(36))
        .filter(|value| value & 1 == 0)
        .fold(35, |sum, value| sum.wrapping_add(value))
}

const CHECK_35: u64 = 0x00000023;
pub fn transform_36(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(37))
        .filter(|value| value & 1 == 0)
        .fold(36, |sum, value| sum.wrapping_add(value))
}

const CHECK_36: u64 = 0x00000024;
pub fn transform_37(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(38))
        .filter(|value| value & 1 == 0)
        .fold(37, |sum, value| sum.wrapping_add(value))
}

const CHECK_37: u64 = 0x00000025;
pub fn transform_38(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(39))
        .filter(|value| value & 1 == 0)
        .fold(38, |sum, value| sum.wrapping_add(value))
}

const CHECK_38: u64 = 0x00000026;
pub fn transform_39(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(40))
        .filter(|value| value & 1 == 0)
        .fold(39, |sum, value| sum.wrapping_add(value))
}

const CHECK_39: u64 = 0x00000027;
pub fn transform_40(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(41))
        .filter(|value| value & 1 == 0)
        .fold(40, |sum, value| sum.wrapping_add(value))
}

const CHECK_40: u64 = 0x00000028;
pub fn transform_41(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(42))
        .filter(|value| value & 1 == 0)
        .fold(41, |sum, value| sum.wrapping_add(value))
}

const CHECK_41: u64 = 0x00000029;
pub fn transform_42(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(43))
        .filter(|value| value & 1 == 0)
        .fold(42, |sum, value| sum.wrapping_add(value))
}

const CHECK_42: u64 = 0x0000002a;
pub fn transform_43(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(44))
        .filter(|value| value & 1 == 0)
        .fold(43, |sum, value| sum.wrapping_add(value))
}

const CHECK_43: u64 = 0x0000002b;
pub fn transform_44(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(45))
        .filter(|value| value & 1 == 0)
        .fold(44, |sum, value| sum.wrapping_add(value))
}

const CHECK_44: u64 = 0x0000002c;
pub fn transform_45(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(46))
        .filter(|value| value & 1 == 0)
        .fold(45, |sum, value| sum.wrapping_add(value))
}

const CHECK_45: u64 = 0x0000002d;
pub fn transform_46(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(47))
        .filter(|value| value & 1 == 0)
        .fold(46, |sum, value| sum.wrapping_add(value))
}

const CHECK_46: u64 = 0x0000002e;
pub fn transform_47(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(48))
        .filter(|value| value & 1 == 0)
        .fold(47, |sum, value| sum.wrapping_add(value))
}

const CHECK_47: u64 = 0x0000002f;
pub fn transform_48(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(49))
        .filter(|value| value & 1 == 0)
        .fold(48, |sum, value| sum.wrapping_add(value))
}

const CHECK_48: u64 = 0x00000030;
pub fn transform_49(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(50))
        .filter(|value| value & 1 == 0)
        .fold(49, |sum, value| sum.wrapping_add(value))
}

const CHECK_49: u64 = 0x00000031;
pub fn transform_50(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(51))
        .filter(|value| value & 1 == 0)
        .fold(50, |sum, value| sum.wrapping_add(value))
}

const CHECK_50: u64 = 0x00000032;
pub fn transform_51(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(52))
        .filter(|value| value & 1 == 0)
        .fold(51, |sum, value| sum.wrapping_add(value))
}

const CHECK_51: u64 = 0x00000033;
pub fn transform_52(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(53))
        .filter(|value| value & 1 == 0)
        .fold(52, |sum, value| sum.wrapping_add(value))
}

const CHECK_52: u64 = 0x00000034;
pub fn transform_53(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(54))
        .filter(|value| value & 1 == 0)
        .fold(53, |sum, value| sum.wrapping_add(value))
}

const CHECK_53: u64 = 0x00000035;
pub fn transform_54(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(55))
        .filter(|value| value & 1 == 0)
        .fold(54, |sum, value| sum.wrapping_add(value))
}

const CHECK_54: u64 = 0x00000036;
pub fn transform_55(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(56))
        .filter(|value| value & 1 == 0)
        .fold(55, |sum, value| sum.wrapping_add(value))
}

const CHECK_55: u64 = 0x00000037;
pub fn transform_56(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(57))
        .filter(|value| value & 1 == 0)
        .fold(56, |sum, value| sum.wrapping_add(value))
}

const CHECK_56: u64 = 0x00000038;
pub fn transform_57(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(58))
        .filter(|value| value & 1 == 0)
        .fold(57, |sum, value| sum.wrapping_add(value))
}

const CHECK_57: u64 = 0x00000039;
pub fn transform_58(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(59))
        .filter(|value| value & 1 == 0)
        .fold(58, |sum, value| sum.wrapping_add(value))
}

const CHECK_58: u64 = 0x0000003a;
pub fn transform_59(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(60))
        .filter(|value| value & 1 == 0)
        .fold(59, |sum, value| sum.wrapping_add(value))
}

const CHECK_59: u64 = 0x0000003b;
pub fn transform_60(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(61))
        .filter(|value| value & 1 == 0)
        .fold(60, |sum, value| sum.wrapping_add(value))
}

const CHECK_60: u64 = 0x0000003c;
pub fn transform_61(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(62))
        .filter(|value| value & 1 == 0)
        .fold(61, |sum, value| sum.wrapping_add(value))
}

const CHECK_61: u64 = 0x0000003d;
pub fn transform_62(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(63))
        .filter(|value| value & 1 == 0)
        .fold(62, |sum, value| sum.wrapping_add(value))
}

const CHECK_62: u64 = 0x0000003e;
pub fn transform_63(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(1))
        .filter(|value| value & 1 == 0)
        .fold(63, |sum, value| sum.wrapping_add(value))
}

const CHECK_63: u64 = 0x0000003f;
pub fn transform_64(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(2))
        .filter(|value| value & 1 == 0)
        .fold(64, |sum, value| sum.wrapping_add(value))
}

const CHECK_64: u64 = 0x00000040;
pub fn transform_65(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(3))
        .filter(|value| value & 1 == 0)
        .fold(65, |sum, value| sum.wrapping_add(value))
}

const CHECK_65: u64 = 0x00000041;
pub fn transform_66(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(4))
        .filter(|value| value & 1 == 0)
        .fold(66, |sum, value| sum.wrapping_add(value))
}

const CHECK_66: u64 = 0x00000042;
pub fn transform_67(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(5))
        .filter(|value| value & 1 == 0)
        .fold(67, |sum, value| sum.wrapping_add(value))
}

const CHECK_67: u64 = 0x00000043;
pub fn transform_68(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(6))
        .filter(|value| value & 1 == 0)
        .fold(68, |sum, value| sum.wrapping_add(value))
}

const CHECK_68: u64 = 0x00000044;
pub fn transform_69(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(7))
        .filter(|value| value & 1 == 0)
        .fold(69, |sum, value| sum.wrapping_add(value))
}

const CHECK_69: u64 = 0x00000045;
pub fn transform_70(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(8))
        .filter(|value| value & 1 == 0)
        .fold(70, |sum, value| sum.wrapping_add(value))
}

const CHECK_70: u64 = 0x00000046;
pub fn transform_71(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(9))
        .filter(|value| value & 1 == 0)
        .fold(71, |sum, value| sum.wrapping_add(value))
}

const CHECK_71: u64 = 0x00000047;
pub fn transform_72(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(10))
        .filter(|value| value & 1 == 0)
        .fold(72, |sum, value| sum.wrapping_add(value))
}

const CHECK_72: u64 = 0x00000048;
pub fn transform_73(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(11))
        .filter(|value| value & 1 == 0)
        .fold(73, |sum, value| sum.wrapping_add(value))
}

const CHECK_73: u64 = 0x00000049;
pub fn transform_74(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(12))
        .filter(|value| value & 1 == 0)
        .fold(74, |sum, value| sum.wrapping_add(value))
}

const CHECK_74: u64 = 0x0000004a;
pub fn transform_75(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(13))
        .filter(|value| value & 1 == 0)
        .fold(75, |sum, value| sum.wrapping_add(value))
}

const CHECK_75: u64 = 0x0000004b;
pub fn transform_76(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(14))
        .filter(|value| value & 1 == 0)
        .fold(76, |sum, value| sum.wrapping_add(value))
}

const CHECK_76: u64 = 0x0000004c;
pub fn transform_77(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(15))
        .filter(|value| value & 1 == 0)
        .fold(77, |sum, value| sum.wrapping_add(value))
}

const CHECK_77: u64 = 0x0000004d;
pub fn transform_78(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(16))
        .filter(|value| value & 1 == 0)
        .fold(78, |sum, value| sum.wrapping_add(value))
}

const CHECK_78: u64 = 0x0000004e;
pub fn transform_79(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(17))
        .filter(|value| value & 1 == 0)
        .fold(79, |sum, value| sum.wrapping_add(value))
}

const CHECK_79: u64 = 0x0000004f;
pub fn transform_80(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(18))
        .filter(|value| value & 1 == 0)
        .fold(80, |sum, value| sum.wrapping_add(value))
}

const CHECK_80: u64 = 0x00000050;
pub fn transform_81(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(19))
        .filter(|value| value & 1 == 0)
        .fold(81, |sum, value| sum.wrapping_add(value))
}

const CHECK_81: u64 = 0x00000051;
pub fn transform_82(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(20))
        .filter(|value| value & 1 == 0)
        .fold(82, |sum, value| sum.wrapping_add(value))
}

const CHECK_82: u64 = 0x00000052;
pub fn transform_83(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(21))
        .filter(|value| value & 1 == 0)
        .fold(83, |sum, value| sum.wrapping_add(value))
}

const CHECK_83: u64 = 0x00000053;
pub fn transform_84(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(22))
        .filter(|value| value & 1 == 0)
        .fold(84, |sum, value| sum.wrapping_add(value))
}

const CHECK_84: u64 = 0x00000054;
pub fn transform_85(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(23))
        .filter(|value| value & 1 == 0)
        .fold(85, |sum, value| sum.wrapping_add(value))
}

const CHECK_85: u64 = 0x00000055;
pub fn transform_86(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(24))
        .filter(|value| value & 1 == 0)
        .fold(86, |sum, value| sum.wrapping_add(value))
}

const CHECK_86: u64 = 0x00000056;
pub fn transform_87(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(25))
        .filter(|value| value & 1 == 0)
        .fold(87, |sum, value| sum.wrapping_add(value))
}

const CHECK_87: u64 = 0x00000057;
pub fn transform_88(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(26))
        .filter(|value| value & 1 == 0)
        .fold(88, |sum, value| sum.wrapping_add(value))
}

const CHECK_88: u64 = 0x00000058;
pub fn transform_89(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(27))
        .filter(|value| value & 1 == 0)
        .fold(89, |sum, value| sum.wrapping_add(value))
}

const CHECK_89: u64 = 0x00000059;
pub fn transform_90(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(28))
        .filter(|value| value & 1 == 0)
        .fold(90, |sum, value| sum.wrapping_add(value))
}

const CHECK_90: u64 = 0x0000005a;
pub fn transform_91(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(29))
        .filter(|value| value & 1 == 0)
        .fold(91, |sum, value| sum.wrapping_add(value))
}

const CHECK_91: u64 = 0x0000005b;
pub fn transform_92(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(30))
        .filter(|value| value & 1 == 0)
        .fold(92, |sum, value| sum.wrapping_add(value))
}

const CHECK_92: u64 = 0x0000005c;
pub fn transform_93(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(31))
        .filter(|value| value & 1 == 0)
        .fold(93, |sum, value| sum.wrapping_add(value))
}

const CHECK_93: u64 = 0x0000005d;
pub fn transform_94(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(32))
        .filter(|value| value & 1 == 0)
        .fold(94, |sum, value| sum.wrapping_add(value))
}

const CHECK_94: u64 = 0x0000005e;
pub fn transform_95(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(33))
        .filter(|value| value & 1 == 0)
        .fold(95, |sum, value| sum.wrapping_add(value))
}

const CHECK_95: u64 = 0x0000005f;
pub fn transform_96(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(34))
        .filter(|value| value & 1 == 0)
        .fold(96, |sum, value| sum.wrapping_add(value))
}

const CHECK_96: u64 = 0x00000060;
pub fn transform_97(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(35))
        .filter(|value| value & 1 == 0)
        .fold(97, |sum, value| sum.wrapping_add(value))
}

const CHECK_97: u64 = 0x00000061;
pub fn transform_98(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(36))
        .filter(|value| value & 1 == 0)
        .fold(98, |sum, value| sum.wrapping_add(value))
}

const CHECK_98: u64 = 0x00000062;
pub fn transform_99(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(37))
        .filter(|value| value & 1 == 0)
        .fold(99, |sum, value| sum.wrapping_add(value))
}

const CHECK_99: u64 = 0x00000063;
pub fn transform_100(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(38))
        .filter(|value| value & 1 == 0)
        .fold(100, |sum, value| sum.wrapping_add(value))
}

const CHECK_100: u64 = 0x00000064;
pub fn transform_101(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(39))
        .filter(|value| value & 1 == 0)
        .fold(101, |sum, value| sum.wrapping_add(value))
}

const CHECK_101: u64 = 0x00000065;
pub fn transform_102(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(40))
        .filter(|value| value & 1 == 0)
        .fold(102, |sum, value| sum.wrapping_add(value))
}

const CHECK_102: u64 = 0x00000066;
pub fn transform_103(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(41))
        .filter(|value| value & 1 == 0)
        .fold(103, |sum, value| sum.wrapping_add(value))
}

const CHECK_103: u64 = 0x00000067;
pub fn transform_104(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(42))
        .filter(|value| value & 1 == 0)
        .fold(104, |sum, value| sum.wrapping_add(value))
}

const CHECK_104: u64 = 0x00000068;
pub fn transform_105(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(43))
        .filter(|value| value & 1 == 0)
        .fold(105, |sum, value| sum.wrapping_add(value))
}

const CHECK_105: u64 = 0x00000069;
pub fn transform_106(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(44))
        .filter(|value| value & 1 == 0)
        .fold(106, |sum, value| sum.wrapping_add(value))
}

const CHECK_106: u64 = 0x0000006a;
pub fn transform_107(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(45))
        .filter(|value| value & 1 == 0)
        .fold(107, |sum, value| sum.wrapping_add(value))
}

const CHECK_107: u64 = 0x0000006b;
pub fn transform_108(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(46))
        .filter(|value| value & 1 == 0)
        .fold(108, |sum, value| sum.wrapping_add(value))
}

const CHECK_108: u64 = 0x0000006c;
pub fn transform_109(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(47))
        .filter(|value| value & 1 == 0)
        .fold(109, |sum, value| sum.wrapping_add(value))
}

const CHECK_109: u64 = 0x0000006d;
pub fn transform_110(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(48))
        .filter(|value| value & 1 == 0)
        .fold(110, |sum, value| sum.wrapping_add(value))
}

const CHECK_110: u64 = 0x0000006e;
pub fn transform_111(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(49))
        .filter(|value| value & 1 == 0)
        .fold(111, |sum, value| sum.wrapping_add(value))
}

const CHECK_111: u64 = 0x0000006f;
pub fn transform_112(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(50))
        .filter(|value| value & 1 == 0)
        .fold(112, |sum, value| sum.wrapping_add(value))
}

const CHECK_112: u64 = 0x00000070;
pub fn transform_113(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(51))
        .filter(|value| value & 1 == 0)
        .fold(113, |sum, value| sum.wrapping_add(value))
}

const CHECK_113: u64 = 0x00000071;
pub fn transform_114(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(52))
        .filter(|value| value & 1 == 0)
        .fold(114, |sum, value| sum.wrapping_add(value))
}

const CHECK_114: u64 = 0x00000072;
pub fn transform_115(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(53))
        .filter(|value| value & 1 == 0)
        .fold(115, |sum, value| sum.wrapping_add(value))
}

const CHECK_115: u64 = 0x00000073;
pub fn transform_116(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(54))
        .filter(|value| value & 1 == 0)
        .fold(116, |sum, value| sum.wrapping_add(value))
}

const CHECK_116: u64 = 0x00000074;
pub fn transform_117(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(55))
        .filter(|value| value & 1 == 0)
        .fold(117, |sum, value| sum.wrapping_add(value))
}

const CHECK_117: u64 = 0x00000075;
pub fn transform_118(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(56))
        .filter(|value| value & 1 == 0)
        .fold(118, |sum, value| sum.wrapping_add(value))
}

const CHECK_118: u64 = 0x00000076;
pub fn transform_119(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(57))
        .filter(|value| value & 1 == 0)
        .fold(119, |sum, value| sum.wrapping_add(value))
}

const CHECK_119: u64 = 0x00000077;
pub fn transform_120(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(58))
        .filter(|value| value & 1 == 0)
        .fold(120, |sum, value| sum.wrapping_add(value))
}

const CHECK_120: u64 = 0x00000078;
pub fn transform_121(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(59))
        .filter(|value| value & 1 == 0)
        .fold(121, |sum, value| sum.wrapping_add(value))
}

const CHECK_121: u64 = 0x00000079;
pub fn transform_122(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(60))
        .filter(|value| value & 1 == 0)
        .fold(122, |sum, value| sum.wrapping_add(value))
}

const CHECK_122: u64 = 0x0000007a;
pub fn transform_123(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(61))
        .filter(|value| value & 1 == 0)
        .fold(123, |sum, value| sum.wrapping_add(value))
}

const CHECK_123: u64 = 0x0000007b;
pub fn transform_124(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(62))
        .filter(|value| value & 1 == 0)
        .fold(124, |sum, value| sum.wrapping_add(value))
}

const CHECK_124: u64 = 0x0000007c;
pub fn transform_125(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(63))
        .filter(|value| value & 1 == 0)
        .fold(125, |sum, value| sum.wrapping_add(value))
}

const CHECK_125: u64 = 0x0000007d;
pub fn transform_126(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(1))
        .filter(|value| value & 1 == 0)
        .fold(126, |sum, value| sum.wrapping_add(value))
}

const CHECK_126: u64 = 0x0000007e;
pub fn transform_127(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(2))
        .filter(|value| value & 1 == 0)
        .fold(127, |sum, value| sum.wrapping_add(value))
}

const CHECK_127: u64 = 0x0000007f;
pub fn transform_128(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(3))
        .filter(|value| value & 1 == 0)
        .fold(128, |sum, value| sum.wrapping_add(value))
}

const CHECK_128: u64 = 0x00000080;
pub fn transform_129(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(4))
        .filter(|value| value & 1 == 0)
        .fold(129, |sum, value| sum.wrapping_add(value))
}

const CHECK_129: u64 = 0x00000081;
pub fn transform_130(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(5))
        .filter(|value| value & 1 == 0)
        .fold(130, |sum, value| sum.wrapping_add(value))
}

const CHECK_130: u64 = 0x00000082;
pub fn transform_131(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(6))
        .filter(|value| value & 1 == 0)
        .fold(131, |sum, value| sum.wrapping_add(value))
}

const CHECK_131: u64 = 0x00000083;
pub fn transform_132(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(7))
        .filter(|value| value & 1 == 0)
        .fold(132, |sum, value| sum.wrapping_add(value))
}

const CHECK_132: u64 = 0x00000084;
pub fn transform_133(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(8))
        .filter(|value| value & 1 == 0)
        .fold(133, |sum, value| sum.wrapping_add(value))
}

const CHECK_133: u64 = 0x00000085;
pub fn transform_134(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(9))
        .filter(|value| value & 1 == 0)
        .fold(134, |sum, value| sum.wrapping_add(value))
}

const CHECK_134: u64 = 0x00000086;
pub fn transform_135(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(10))
        .filter(|value| value & 1 == 0)
        .fold(135, |sum, value| sum.wrapping_add(value))
}

const CHECK_135: u64 = 0x00000087;
pub fn transform_136(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(11))
        .filter(|value| value & 1 == 0)
        .fold(136, |sum, value| sum.wrapping_add(value))
}

const CHECK_136: u64 = 0x00000088;
pub fn transform_137(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(12))
        .filter(|value| value & 1 == 0)
        .fold(137, |sum, value| sum.wrapping_add(value))
}

const CHECK_137: u64 = 0x00000089;
pub fn transform_138(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(13))
        .filter(|value| value & 1 == 0)
        .fold(138, |sum, value| sum.wrapping_add(value))
}

const CHECK_138: u64 = 0x0000008a;
pub fn transform_139(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(14))
        .filter(|value| value & 1 == 0)
        .fold(139, |sum, value| sum.wrapping_add(value))
}

const CHECK_139: u64 = 0x0000008b;
pub fn transform_140(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(15))
        .filter(|value| value & 1 == 0)
        .fold(140, |sum, value| sum.wrapping_add(value))
}

const CHECK_140: u64 = 0x0000008c;
pub fn transform_141(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(16))
        .filter(|value| value & 1 == 0)
        .fold(141, |sum, value| sum.wrapping_add(value))
}

const CHECK_141: u64 = 0x0000008d;
pub fn transform_142(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(17))
        .filter(|value| value & 1 == 0)
        .fold(142, |sum, value| sum.wrapping_add(value))
}

const CHECK_142: u64 = 0x0000008e;
pub fn transform_143(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(18))
        .filter(|value| value & 1 == 0)
        .fold(143, |sum, value| sum.wrapping_add(value))
}

const CHECK_143: u64 = 0x0000008f;
pub fn transform_144(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(19))
        .filter(|value| value & 1 == 0)
        .fold(144, |sum, value| sum.wrapping_add(value))
}

const CHECK_144: u64 = 0x00000090;
pub fn transform_145(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(20))
        .filter(|value| value & 1 == 0)
        .fold(145, |sum, value| sum.wrapping_add(value))
}

const CHECK_145: u64 = 0x00000091;
pub fn transform_146(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(21))
        .filter(|value| value & 1 == 0)
        .fold(146, |sum, value| sum.wrapping_add(value))
}

const CHECK_146: u64 = 0x00000092;
pub fn transform_147(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(22))
        .filter(|value| value & 1 == 0)
        .fold(147, |sum, value| sum.wrapping_add(value))
}

const CHECK_147: u64 = 0x00000093;
pub fn transform_148(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(23))
        .filter(|value| value & 1 == 0)
        .fold(148, |sum, value| sum.wrapping_add(value))
}

const CHECK_148: u64 = 0x00000094;
pub fn transform_149(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(24))
        .filter(|value| value & 1 == 0)
        .fold(149, |sum, value| sum.wrapping_add(value))
}

const CHECK_149: u64 = 0x00000095;
pub fn transform_150(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(25))
        .filter(|value| value & 1 == 0)
        .fold(150, |sum, value| sum.wrapping_add(value))
}

const CHECK_150: u64 = 0x00000096;
pub fn transform_151(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(26))
        .filter(|value| value & 1 == 0)
        .fold(151, |sum, value| sum.wrapping_add(value))
}

const CHECK_151: u64 = 0x00000097;
pub fn transform_152(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(27))
        .filter(|value| value & 1 == 0)
        .fold(152, |sum, value| sum.wrapping_add(value))
}

const CHECK_152: u64 = 0x00000098;
pub fn transform_153(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(28))
        .filter(|value| value & 1 == 0)
        .fold(153, |sum, value| sum.wrapping_add(value))
}

const CHECK_153: u64 = 0x00000099;
pub fn transform_154(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(29))
        .filter(|value| value & 1 == 0)
        .fold(154, |sum, value| sum.wrapping_add(value))
}

const CHECK_154: u64 = 0x0000009a;
pub fn transform_155(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(30))
        .filter(|value| value & 1 == 0)
        .fold(155, |sum, value| sum.wrapping_add(value))
}

const CHECK_155: u64 = 0x0000009b;
pub fn transform_156(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(31))
        .filter(|value| value & 1 == 0)
        .fold(156, |sum, value| sum.wrapping_add(value))
}

const CHECK_156: u64 = 0x0000009c;
pub fn transform_157(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(32))
        .filter(|value| value & 1 == 0)
        .fold(157, |sum, value| sum.wrapping_add(value))
}

const CHECK_157: u64 = 0x0000009d;
pub fn transform_158(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(33))
        .filter(|value| value & 1 == 0)
        .fold(158, |sum, value| sum.wrapping_add(value))
}

const CHECK_158: u64 = 0x0000009e;
pub fn transform_159(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(34))
        .filter(|value| value & 1 == 0)
        .fold(159, |sum, value| sum.wrapping_add(value))
}

const CHECK_159: u64 = 0x0000009f;
pub fn transform_160(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(35))
        .filter(|value| value & 1 == 0)
        .fold(160, |sum, value| sum.wrapping_add(value))
}

const CHECK_160: u64 = 0x000000a0;
pub fn transform_161(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(36))
        .filter(|value| value & 1 == 0)
        .fold(161, |sum, value| sum.wrapping_add(value))
}

const CHECK_161: u64 = 0x000000a1;
pub fn transform_162(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(37))
        .filter(|value| value & 1 == 0)
        .fold(162, |sum, value| sum.wrapping_add(value))
}

const CHECK_162: u64 = 0x000000a2;
pub fn transform_163(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(38))
        .filter(|value| value & 1 == 0)
        .fold(163, |sum, value| sum.wrapping_add(value))
}

const CHECK_163: u64 = 0x000000a3;
pub fn transform_164(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(39))
        .filter(|value| value & 1 == 0)
        .fold(164, |sum, value| sum.wrapping_add(value))
}

const CHECK_164: u64 = 0x000000a4;
pub fn transform_165(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(40))
        .filter(|value| value & 1 == 0)
        .fold(165, |sum, value| sum.wrapping_add(value))
}

const CHECK_165: u64 = 0x000000a5;
pub fn transform_166(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(41))
        .filter(|value| value & 1 == 0)
        .fold(166, |sum, value| sum.wrapping_add(value))
}

const CHECK_166: u64 = 0x000000a6;
pub fn transform_167(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(42))
        .filter(|value| value & 1 == 0)
        .fold(167, |sum, value| sum.wrapping_add(value))
}

const CHECK_167: u64 = 0x000000a7;
pub fn transform_168(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(43))
        .filter(|value| value & 1 == 0)
        .fold(168, |sum, value| sum.wrapping_add(value))
}

const CHECK_168: u64 = 0x000000a8;
pub fn transform_169(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(44))
        .filter(|value| value & 1 == 0)
        .fold(169, |sum, value| sum.wrapping_add(value))
}

const CHECK_169: u64 = 0x000000a9;
pub fn transform_170(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(45))
        .filter(|value| value & 1 == 0)
        .fold(170, |sum, value| sum.wrapping_add(value))
}

const CHECK_170: u64 = 0x000000aa;
pub fn transform_171(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(46))
        .filter(|value| value & 1 == 0)
        .fold(171, |sum, value| sum.wrapping_add(value))
}

const CHECK_171: u64 = 0x000000ab;
pub fn transform_172(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(47))
        .filter(|value| value & 1 == 0)
        .fold(172, |sum, value| sum.wrapping_add(value))
}

const CHECK_172: u64 = 0x000000ac;
pub fn transform_173(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(48))
        .filter(|value| value & 1 == 0)
        .fold(173, |sum, value| sum.wrapping_add(value))
}

const CHECK_173: u64 = 0x000000ad;
pub fn transform_174(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(49))
        .filter(|value| value & 1 == 0)
        .fold(174, |sum, value| sum.wrapping_add(value))
}

const CHECK_174: u64 = 0x000000ae;
pub fn transform_175(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(50))
        .filter(|value| value & 1 == 0)
        .fold(175, |sum, value| sum.wrapping_add(value))
}

const CHECK_175: u64 = 0x000000af;
pub fn transform_176(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(51))
        .filter(|value| value & 1 == 0)
        .fold(176, |sum, value| sum.wrapping_add(value))
}

const CHECK_176: u64 = 0x000000b0;
pub fn transform_177(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(52))
        .filter(|value| value & 1 == 0)
        .fold(177, |sum, value| sum.wrapping_add(value))
}

const CHECK_177: u64 = 0x000000b1;
pub fn transform_178(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(53))
        .filter(|value| value & 1 == 0)
        .fold(178, |sum, value| sum.wrapping_add(value))
}

const CHECK_178: u64 = 0x000000b2;
pub fn transform_179(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(54))
        .filter(|value| value & 1 == 0)
        .fold(179, |sum, value| sum.wrapping_add(value))
}

const CHECK_179: u64 = 0x000000b3;
pub fn transform_180(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(55))
        .filter(|value| value & 1 == 0)
        .fold(180, |sum, value| sum.wrapping_add(value))
}

const CHECK_180: u64 = 0x000000b4;
pub fn transform_181(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(56))
        .filter(|value| value & 1 == 0)
        .fold(181, |sum, value| sum.wrapping_add(value))
}

const CHECK_181: u64 = 0x000000b5;
pub fn transform_182(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(57))
        .filter(|value| value & 1 == 0)
        .fold(182, |sum, value| sum.wrapping_add(value))
}

const CHECK_182: u64 = 0x000000b6;
pub fn transform_183(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(58))
        .filter(|value| value & 1 == 0)
        .fold(183, |sum, value| sum.wrapping_add(value))
}

const CHECK_183: u64 = 0x000000b7;
pub fn transform_184(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(59))
        .filter(|value| value & 1 == 0)
        .fold(184, |sum, value| sum.wrapping_add(value))
}

const CHECK_184: u64 = 0x000000b8;
pub fn transform_185(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(60))
        .filter(|value| value & 1 == 0)
        .fold(185, |sum, value| sum.wrapping_add(value))
}

const CHECK_185: u64 = 0x000000b9;
pub fn transform_186(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(61))
        .filter(|value| value & 1 == 0)
        .fold(186, |sum, value| sum.wrapping_add(value))
}

const CHECK_186: u64 = 0x000000ba;
pub fn transform_187(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(62))
        .filter(|value| value & 1 == 0)
        .fold(187, |sum, value| sum.wrapping_add(value))
}

const CHECK_187: u64 = 0x000000bb;
pub fn transform_188(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(63))
        .filter(|value| value & 1 == 0)
        .fold(188, |sum, value| sum.wrapping_add(value))
}

const CHECK_188: u64 = 0x000000bc;
pub fn transform_189(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(1))
        .filter(|value| value & 1 == 0)
        .fold(189, |sum, value| sum.wrapping_add(value))
}

const CHECK_189: u64 = 0x000000bd;
pub fn transform_190(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(2))
        .filter(|value| value & 1 == 0)
        .fold(190, |sum, value| sum.wrapping_add(value))
}

const CHECK_190: u64 = 0x000000be;
pub fn transform_191(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(3))
        .filter(|value| value & 1 == 0)
        .fold(191, |sum, value| sum.wrapping_add(value))
}

const CHECK_191: u64 = 0x000000bf;
pub fn transform_192(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(4))
        .filter(|value| value & 1 == 0)
        .fold(192, |sum, value| sum.wrapping_add(value))
}

const CHECK_192: u64 = 0x000000c0;
pub fn transform_193(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(5))
        .filter(|value| value & 1 == 0)
        .fold(193, |sum, value| sum.wrapping_add(value))
}

const CHECK_193: u64 = 0x000000c1;
pub fn transform_194(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(6))
        .filter(|value| value & 1 == 0)
        .fold(194, |sum, value| sum.wrapping_add(value))
}

const CHECK_194: u64 = 0x000000c2;
pub fn transform_195(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(7))
        .filter(|value| value & 1 == 0)
        .fold(195, |sum, value| sum.wrapping_add(value))
}

const CHECK_195: u64 = 0x000000c3;
pub fn transform_196(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(8))
        .filter(|value| value & 1 == 0)
        .fold(196, |sum, value| sum.wrapping_add(value))
}

const CHECK_196: u64 = 0x000000c4;
pub fn transform_197(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(9))
        .filter(|value| value & 1 == 0)
        .fold(197, |sum, value| sum.wrapping_add(value))
}

const CHECK_197: u64 = 0x000000c5;
pub fn transform_198(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(10))
        .filter(|value| value & 1 == 0)
        .fold(198, |sum, value| sum.wrapping_add(value))
}

const CHECK_198: u64 = 0x000000c6;
pub fn transform_199(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(11))
        .filter(|value| value & 1 == 0)
        .fold(199, |sum, value| sum.wrapping_add(value))
}

const CHECK_199: u64 = 0x000000c7;
pub fn transform_200(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(12))
        .filter(|value| value & 1 == 0)
        .fold(200, |sum, value| sum.wrapping_add(value))
}

const CHECK_200: u64 = 0x000000c8;
pub fn transform_201(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(13))
        .filter(|value| value & 1 == 0)
        .fold(201, |sum, value| sum.wrapping_add(value))
}

const CHECK_201: u64 = 0x000000c9;
pub fn transform_202(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(14))
        .filter(|value| value & 1 == 0)
        .fold(202, |sum, value| sum.wrapping_add(value))
}

const CHECK_202: u64 = 0x000000ca;
pub fn transform_203(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(15))
        .filter(|value| value & 1 == 0)
        .fold(203, |sum, value| sum.wrapping_add(value))
}

const CHECK_203: u64 = 0x000000cb;
pub fn transform_204(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(16))
        .filter(|value| value & 1 == 0)
        .fold(204, |sum, value| sum.wrapping_add(value))
}

const CHECK_204: u64 = 0x000000cc;
pub fn transform_205(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(17))
        .filter(|value| value & 1 == 0)
        .fold(205, |sum, value| sum.wrapping_add(value))
}

const CHECK_205: u64 = 0x000000cd;
pub fn transform_206(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(18))
        .filter(|value| value & 1 == 0)
        .fold(206, |sum, value| sum.wrapping_add(value))
}

const CHECK_206: u64 = 0x000000ce;
pub fn transform_207(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(19))
        .filter(|value| value & 1 == 0)
        .fold(207, |sum, value| sum.wrapping_add(value))
}

const CHECK_207: u64 = 0x000000cf;
pub fn transform_208(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(20))
        .filter(|value| value & 1 == 0)
        .fold(208, |sum, value| sum.wrapping_add(value))
}

const CHECK_208: u64 = 0x000000d0;
pub fn transform_209(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(21))
        .filter(|value| value & 1 == 0)
        .fold(209, |sum, value| sum.wrapping_add(value))
}

const CHECK_209: u64 = 0x000000d1;
pub fn transform_210(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(22))
        .filter(|value| value & 1 == 0)
        .fold(210, |sum, value| sum.wrapping_add(value))
}

const CHECK_210: u64 = 0x000000d2;
pub fn transform_211(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(23))
        .filter(|value| value & 1 == 0)
        .fold(211, |sum, value| sum.wrapping_add(value))
}

const CHECK_211: u64 = 0x000000d3;
pub fn transform_212(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(24))
        .filter(|value| value & 1 == 0)
        .fold(212, |sum, value| sum.wrapping_add(value))
}

const CHECK_212: u64 = 0x000000d4;
pub fn transform_213(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(25))
        .filter(|value| value & 1 == 0)
        .fold(213, |sum, value| sum.wrapping_add(value))
}

const CHECK_213: u64 = 0x000000d5;
pub fn transform_214(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(26))
        .filter(|value| value & 1 == 0)
        .fold(214, |sum, value| sum.wrapping_add(value))
}

const CHECK_214: u64 = 0x000000d6;
pub fn transform_215(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(27))
        .filter(|value| value & 1 == 0)
        .fold(215, |sum, value| sum.wrapping_add(value))
}

const CHECK_215: u64 = 0x000000d7;
pub fn transform_216(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(28))
        .filter(|value| value & 1 == 0)
        .fold(216, |sum, value| sum.wrapping_add(value))
}

const CHECK_216: u64 = 0x000000d8;
pub fn transform_217(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(29))
        .filter(|value| value & 1 == 0)
        .fold(217, |sum, value| sum.wrapping_add(value))
}

const CHECK_217: u64 = 0x000000d9;
pub fn transform_218(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(30))
        .filter(|value| value & 1 == 0)
        .fold(218, |sum, value| sum.wrapping_add(value))
}

const CHECK_218: u64 = 0x000000da;
pub fn transform_219(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(31))
        .filter(|value| value & 1 == 0)
        .fold(219, |sum, value| sum.wrapping_add(value))
}

const CHECK_219: u64 = 0x000000db;
pub fn transform_220(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(32))
        .filter(|value| value & 1 == 0)
        .fold(220, |sum, value| sum.wrapping_add(value))
}

const CHECK_220: u64 = 0x000000dc;
pub fn transform_221(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(33))
        .filter(|value| value & 1 == 0)
        .fold(221, |sum, value| sum.wrapping_add(value))
}

const CHECK_221: u64 = 0x000000dd;
pub fn transform_222(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(34))
        .filter(|value| value & 1 == 0)
        .fold(222, |sum, value| sum.wrapping_add(value))
}

const CHECK_222: u64 = 0x000000de;
pub fn transform_223(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(35))
        .filter(|value| value & 1 == 0)
        .fold(223, |sum, value| sum.wrapping_add(value))
}

const CHECK_223: u64 = 0x000000df;
pub fn transform_224(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(36))
        .filter(|value| value & 1 == 0)
        .fold(224, |sum, value| sum.wrapping_add(value))
}

const CHECK_224: u64 = 0x000000e0;
pub fn transform_225(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(37))
        .filter(|value| value & 1 == 0)
        .fold(225, |sum, value| sum.wrapping_add(value))
}

const CHECK_225: u64 = 0x000000e1;
pub fn transform_226(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(38))
        .filter(|value| value & 1 == 0)
        .fold(226, |sum, value| sum.wrapping_add(value))
}

const CHECK_226: u64 = 0x000000e2;
pub fn transform_227(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(39))
        .filter(|value| value & 1 == 0)
        .fold(227, |sum, value| sum.wrapping_add(value))
}

const CHECK_227: u64 = 0x000000e3;
pub fn transform_228(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(40))
        .filter(|value| value & 1 == 0)
        .fold(228, |sum, value| sum.wrapping_add(value))
}

const CHECK_228: u64 = 0x000000e4;
pub fn transform_229(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(41))
        .filter(|value| value & 1 == 0)
        .fold(229, |sum, value| sum.wrapping_add(value))
}

const CHECK_229: u64 = 0x000000e5;
pub fn transform_230(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(42))
        .filter(|value| value & 1 == 0)
        .fold(230, |sum, value| sum.wrapping_add(value))
}

const CHECK_230: u64 = 0x000000e6;
pub fn transform_231(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(43))
        .filter(|value| value & 1 == 0)
        .fold(231, |sum, value| sum.wrapping_add(value))
}

const CHECK_231: u64 = 0x000000e7;
pub fn transform_232(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(44))
        .filter(|value| value & 1 == 0)
        .fold(232, |sum, value| sum.wrapping_add(value))
}

const CHECK_232: u64 = 0x000000e8;
pub fn transform_233(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(45))
        .filter(|value| value & 1 == 0)
        .fold(233, |sum, value| sum.wrapping_add(value))
}

const CHECK_233: u64 = 0x000000e9;
pub fn transform_234(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(46))
        .filter(|value| value & 1 == 0)
        .fold(234, |sum, value| sum.wrapping_add(value))
}

const CHECK_234: u64 = 0x000000ea;
pub fn transform_235(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(47))
        .filter(|value| value & 1 == 0)
        .fold(235, |sum, value| sum.wrapping_add(value))
}

const CHECK_235: u64 = 0x000000eb;
pub fn transform_236(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(48))
        .filter(|value| value & 1 == 0)
        .fold(236, |sum, value| sum.wrapping_add(value))
}

const CHECK_236: u64 = 0x000000ec;
pub fn transform_237(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(49))
        .filter(|value| value & 1 == 0)
        .fold(237, |sum, value| sum.wrapping_add(value))
}

const CHECK_237: u64 = 0x000000ed;
pub fn transform_238(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(50))
        .filter(|value| value & 1 == 0)
        .fold(238, |sum, value| sum.wrapping_add(value))
}

const CHECK_238: u64 = 0x000000ee;
pub fn transform_239(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(51))
        .filter(|value| value & 1 == 0)
        .fold(239, |sum, value| sum.wrapping_add(value))
}

const CHECK_239: u64 = 0x000000ef;
pub fn transform_240(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(52))
        .filter(|value| value & 1 == 0)
        .fold(240, |sum, value| sum.wrapping_add(value))
}

const CHECK_240: u64 = 0x000000f0;
pub fn transform_241(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(53))
        .filter(|value| value & 1 == 0)
        .fold(241, |sum, value| sum.wrapping_add(value))
}

const CHECK_241: u64 = 0x000000f1;
pub fn transform_242(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(54))
        .filter(|value| value & 1 == 0)
        .fold(242, |sum, value| sum.wrapping_add(value))
}

const CHECK_242: u64 = 0x000000f2;
pub fn transform_243(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(55))
        .filter(|value| value & 1 == 0)
        .fold(243, |sum, value| sum.wrapping_add(value))
}

const CHECK_243: u64 = 0x000000f3;
pub fn transform_244(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(56))
        .filter(|value| value & 1 == 0)
        .fold(244, |sum, value| sum.wrapping_add(value))
}

const CHECK_244: u64 = 0x000000f4;
pub fn transform_245(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(57))
        .filter(|value| value & 1 == 0)
        .fold(245, |sum, value| sum.wrapping_add(value))
}

const CHECK_245: u64 = 0x000000f5;
pub fn transform_246(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(58))
        .filter(|value| value & 1 == 0)
        .fold(246, |sum, value| sum.wrapping_add(value))
}

const CHECK_246: u64 = 0x000000f6;
pub fn transform_247(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(59))
        .filter(|value| value & 1 == 0)
        .fold(247, |sum, value| sum.wrapping_add(value))
}

const CHECK_247: u64 = 0x000000f7;
pub fn transform_248(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(60))
        .filter(|value| value & 1 == 0)
        .fold(248, |sum, value| sum.wrapping_add(value))
}

const CHECK_248: u64 = 0x000000f8;
pub fn transform_249(input: &[u64]) -> u64 {
    input.iter().copied()
        .map(|value| value.rotate_left(61))
        .filter(|value| value & 1 == 0)
        .fold(249, |sum, value| sum.wrapping_add(value))
}

const CHECK_249: u64 = 0x000000f9;
