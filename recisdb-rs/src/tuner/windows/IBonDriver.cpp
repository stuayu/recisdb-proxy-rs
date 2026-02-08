//
// Created by maleicacid on 2021/09/27.
//

#include "IBonDriver.hpp"

extern "C" {
    IBonDriver2* interface_check_2(IBonDriver * i)
    {
        try { return dynamic_cast<IBonDriver2*>(i); } catch (...) { return nullptr; }
    }
    IBonDriver3* interface_check_3(IBonDriver2 * i)
    {
        try { return dynamic_cast<IBonDriver3*>(i); } catch (...) { return nullptr; }
    }
    const IBonDriver2* interface_check_2_const(const IBonDriver * i)
    {
        try { return dynamic_cast<const IBonDriver2*>(i); } catch (...) { return nullptr; }
    }
    const IBonDriver3* interface_check_3_const(const IBonDriver2 * i)
    {
        try { return dynamic_cast<const IBonDriver3*>(i); } catch (...) { return nullptr; }
    }
}